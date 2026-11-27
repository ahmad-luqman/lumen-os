"""Build the committed mount-allocation fixture (e2fsprogs 1.47.4).

Run: UV_OFFLINE=1 uv run python tools/ext4-allocation-image.py
No build invokes this script. All filesystem features, geometry, UUID,
hash seed, owners and timestamps are pinned; no host files are imported.
The 12 groups exercise sparse backups, reserved GDT, flex_bg and a journal.
Block-mapped files exercise indirect nodes; symlinks and special inodes
exercise i_block's other meanings. Two files share an external xattr.
"""

from __future__ import annotations

import os
import re
import struct
import subprocess
import sys
import tempfile
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools" / "harness"))
from lumen_harness.rootfs import find_e2fsprogs

# zlib-compressed: the 12 MiB image is almost all zeros (about 21 KiB packed).
OUT = ROOT / "crates/lumen-vfs/testdata/allocation.ext4.zz"
UUID = "4c756d65-6e2d-4355-8000-000000000355"
EPOCH = "1700000000"
BS = 1024


def run(*args: object, debug=False) -> bytes:
    env = {**os.environ, "LC_ALL": "C", "E2FSPROGS_FAKE_TIME": EPOCH, "E2FSCK_TIME": EPOCH}
    p = subprocess.run(list(map(str, args)), capture_output=True, env=env, check=False)
    errors = [s for s in p.stderr.splitlines() if s and not re.fullmatch(rb"debugfs \S+ \(.*\)", s)]
    if p.returncode or (debug and errors):
        raise RuntimeError((args, p.returncode, p.stdout, p.stderr))
    return p.stdout


def main() -> None:
    mke2fs, e2fsck = find_e2fsprogs()
    debugfs = mke2fs.parent / "debugfs"
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        image = work / "allocation.ext4"
        run(
            mke2fs,
            "-q",
            "-F",
            "-t",
            "ext4",
            "-b",
            BS,
            "-g",
            "1024",
            "-G",
            "16",
            "-I",
            "256",
            "-N",
            "384",
            "-m",
            "0",
            "-U",
            UUID,
            "-L",
            "lumen-check",
            "-O",
            "none,has_journal,ext_attr,resize_inode,dir_index,filetype,sparse_super,large_file,flex_bg,extra_isize",
            "-E",
            f"hash_seed={UUID},root_owner=0:0,lazy_itable_init=0,lazy_journal_init=0",
            image,
            "12288",
        )

        def command(text: str) -> bytes:
            return run(debugfs, "-w", "-R", text, image, debug=True)

        empty = work / "empty"
        empty.write_bytes(b"")
        data = work / "data"
        data.write_bytes(bytes(range(256)) * 128)
        attr = work / "attr"
        attr.write_bytes(b"external-value-" * 32)
        command("symlink /fast target")
        command("symlink /fast-xattr target")
        command("symlink /slow " + "long-target-" * 12)
        command("mknod char c 1 3")
        command("mknod block b 8 1")
        command("mknod fifo p")
        command(f"write {empty} /socket")
        command("sif /socket mode 0140600")
        command(f"write {data} /indirect")
        for name in ("shared-a", "shared-b"):
            command(f"write {empty} /{name}")
        for name in ("fast-xattr", "shared-a", "shared-b"):
            command(f"ea_set -f {attr} /{name} user.large")

        def inode(name: str) -> int:
            match = re.search(rb"Inode: (\d+)", command(f"stat /{name}"))
            assert match
            return int(match[1])

        numbers = {name: inode(name) for name in ("shared-a", "shared-b", "fast-xattr")}
        raw = bytearray(image.read_bytes())

        def u32(at: int) -> int:
            return struct.unpack_from("<I", raw, at)[0]

        def w32(at: int, value: int) -> None:
            struct.pack_into("<I", raw, at, value)

        def inode_offset(ino: int) -> int:
            group, slot = divmod(ino - 1, u32(BS + 0x28))
            return u32(2 * BS + group * 32 + 8) * BS + slot * 256

        a, b = (inode_offset(numbers[n]) for n in ("shared-a", "shared-b"))
        shared, old = u32(a + 0x68), u32(b + 0x68)
        assert shared and old and shared != old
        assert u32(inode_offset(numbers["fast-xattr"]) + 0x68)
        # Share identical xattr contents, freeing the replaced block and
        # balancing both counts. No checksums are enabled in this fixture.
        w32(b + 0x68, shared)
        w32(shared * BS + 4, 2)
        group, bit = divmod(old - 1, 1024)
        gd = 2 * BS + group * 32
        bitmap = u32(gd) * BS
        assert raw[bitmap + bit // 8] & (1 << (bit % 8))
        raw[bitmap + bit // 8] &= ~(1 << (bit % 8))
        count = struct.unpack_from("<H", raw, gd + 12)[0]
        struct.pack_into("<H", raw, gd + 12, count + 1)
        w32(BS + 12, u32(BS + 12) + 1)
        # Deterministic even if debugfs changes how it timestamps new inodes.
        for ino in range(1, u32(BS) + 1):
            at = inode_offset(ino)
            if struct.unpack_from("<H", raw, at)[0]:
                for field in (8, 12, 16, 0x90):
                    w32(at + field, int(EPOCH))
                for field in (0x84, 0x88, 0x8C, 0x94):
                    w32(at + field, 0)
        directory = u32(inode_offset(2) + 0x28) * BS
        off = directory
        while off < directory + BS:
            length = struct.unpack_from("<H", raw, off + 4)[0]
            assert length >= 8
            if raw[off + 8 : off + 8 + raw[off + 6]] == b"socket":
                raw[off + 7] = 6
            off += length
        image.write_bytes(raw)
        run(e2fsck, "-fn", image)
        OUT.parent.mkdir(parents=True, exist_ok=True)
        OUT.write_bytes(zlib.compress(raw, 9))
        print(f"wrote {OUT.relative_to(ROOT)} ({len(raw)} bytes)")


if __name__ == "__main__":
    main()
