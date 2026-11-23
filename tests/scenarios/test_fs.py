"""POSIX VFS, PageCache, writable ext4 rootfs, tmpfs, devfs, and procfs scenarios."""

import re
import shutil
import subprocess
from pathlib import Path

import pytest
from lumen_harness.calibration import assert_quiet_boot
from lumen_harness.gui import confirmed_keys, type_confirmed
from lumen_harness.klog import unexpected_warnings
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import (
    copy_rootfs,
    debugfs,
    e2fsck_check,
    find_e2fsprogs,
    run_busybox,
    warm_up,
)
from lumen_harness.screen import expect_screen
from lumen_harness.shell import PROMPT, run

SHELLS = r"INFO  shell: shells? on serial.*; type 'help'\n"


def shell(boot, **kwargs):
    m = boot("qemutest", cmdline="lumen.qemu_exit", **kwargs)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def parse_mount(out):
    """`mount`'s output as {target: (fs, dev, mode, used, limit)}, as text:
    every row (a line starting with `/`) has those six cells and a target of
    its own."""
    rows = {}
    for line in out.splitlines():
        cells = line.split()
        if cells and cells[0].startswith("/"):
            assert len(cells) == 6, f"a mount row without 6 cells: {line!r} in\n{out}"
            assert cells[0] not in rows, f"{cells[0]} listed twice in\n{out}"
            rows[cells[0]] = tuple(cells[1:])
    return rows


def mount_table(m):
    """`mount`'s table, run once: `parse_mount`'s."""
    return parse_mount(run(m, "mount"))


def without_quota(table):
    """A `mount_table` as {target: (fs, dev, mode)}."""
    return {target: row[:3] for target, row in table.items()}


def mount_rows(m):
    """`mount`'s table as {target: (fs, dev, mode)}."""
    return without_quota(mount_table(m))


# A tmpfs mount's quota limit in bytes: page_cache::TMPFS_DEFAULT_QUOTA_PAGES
# pages of 4 KiB.
TMPFS_LIMIT = 32 << 20


def quota_in(table, target):
    """The tmpfs mounted at `target`'s quota in `mount_table`'s `table`:
    (used, limit) in bytes."""
    fs, _dev, _mode, used, limit = table[target]
    assert fs == "tmpfs", (target, fs)
    return int(used), int(limit)


def tmpfs_quota(m, target):
    """The tmpfs mounted at `target`'s quota as `mount` shows it now."""
    return quota_in(mount_table(m), target)


def stat_fields(m, path):
    """`vfstest stat`'s `key=value` fields."""
    out = run(m, f"vfstest stat {path}")
    fields = dict(f.split("=", 1) for f in out.split() if "=" in f)
    assert "ino" in fields, out
    return fields


def blk_dirty(m):
    """How many blocks the block cache holds dirty (`blk`'s dirty column,
    the last row's)."""
    return int(run(m, "blk | select dirty").strip().splitlines()[-1])


def test_ext4_read_write_persists_across_reboot(boot, tmp_path):
    disk = copy_rootfs(tmp_path)

    # Boot 1: attach Alpine ext4 rootfs RW, verify mount table, existing files,
    # write files/dirs/symlinks/chmod/truncate, sync, then abruptly kill QEMU.
    m1 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m1.output)

    table = mount_table(m1)
    assert without_quota(table) == {
        "/": ("ext4", "0:1", "rw"),
        "/tmp": ("tmpfs", "0:2", "rw"),
        "/dev": ("devfs", "0:3", "rw"),
        "/proc": ("procfs", "0:4", "ro"),
    }
    assert table["/"][3:] == ("-", "-")  # ext4 has no quota to show
    assert run(m1, "mount | count") == "4\n"

    assert run(m1, "vfstest cat /etc/alpine-release").strip() == "3.21.10"
    assert run(m1, "vfstest readlink /bin/sh").strip() == "/bin/busybox"

    assert run(m1, "vfstest mkdir /persist_dir 0755") == "mkdir: ok\n"
    assert run(m1, "vfstest mkdir /persist_dir/sub_old 0755") == "mkdir: ok\n"
    assert run(m1, "vfstest rename /persist_dir/sub_old /persist_dir/sub_new") == "rename: ok\n"
    assert run(m1, "vfstest write /persist_dir/hello.txt persisted-ext4-v2") == "wrote 17\n"
    assert run(m1, "vfstest chmod /persist_dir/hello.txt 0640") == "chmod: ok\n"
    assert run(m1, "vfstest write-pattern /persist_dir/big.bin 9001") == "wrote 9001\n"
    assert run(m1, "vfstest write-pattern /persist_dir/trunc.bin 5000") == "wrote 5000\n"
    assert run(m1, "vfstest truncate /persist_dir/trunc.bin 123") == "truncate: ok\n"
    # Shrinking a synced file zeroes its last block's tail on disk too: after
    # the reboot, extending it again shows zeroes from byte 100 on.
    assert run(m1, "vfstest write-pattern /persist_dir/tail.bin 5000") == "wrote 5000\n"
    assert run(m1, "sync") == "sync: ok\n"
    assert run(m1, "vfstest truncate /persist_dir/tail.bin 100") == "truncate: ok\n"
    assert run(m1, "vfstest symlink /persist_link /persist_dir/hello.txt") == "symlink: ok\n"
    long_target = "/persist_dir/0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
    assert len(long_target) >= 60
    assert run(m1, f"vfstest symlink /long_sym {long_target}") == "symlink: ok\n"

    assert run(m1, "sync") == "sync: ok\n"
    assert blk_dirty(m1) == 0
    mtime_before = stat_fields(m1, "/persist_dir/hello.txt")["mtime"]

    # Abrupt power loss after sync (no graceful shell exit).
    m1.close()

    # Host e2fsck -fn must pass cleanly on the mutated image.
    e2fsck_check(disk)

    # Boot 2: reboot on the same disk image and verify persistence.
    m2 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m2.output)

    assert run(m2, "vfstest cat /etc/alpine-release").strip() == "3.21.10"
    assert run(m2, "vfstest cat /persist_dir/hello.txt").strip() == "persisted-ext4-v2"
    st_hello = run(m2, "vfstest stat /persist_dir/hello.txt")
    assert (
        "dev=0:1" in st_hello
        and "mode=0640" in st_hello
        and "size=17" in st_hello
        and "nlink=1" in st_hello
    ), st_hello
    assert (
        run(m2, "vfstest verify-pattern /persist_dir/big.bin 9001") == "verify-pattern: ok (9001)\n"
    )
    assert stat_fields(m2, "/persist_dir/hello.txt")["mtime"] == mtime_before
    st_trunc = run(m2, "vfstest stat /persist_dir/trunc.bin")
    assert "size=123" in st_trunc, st_trunc
    assert (
        run(m2, "vfstest verify-pattern /persist_dir/trunc.bin 123") == "verify-pattern: ok (123)\n"
    )
    # Extend before anything reads tail.bin: a read would cache its first page
    # (zero past the end) and hide what the disk holds.
    assert stat_fields(m2, "/persist_dir/tail.bin")["size"] == "100"
    assert run(m2, "vfstest truncate /persist_dir/tail.bin 5000") == "truncate: ok\n"
    assert (
        run(m2, "vfstest verify-pattern /persist_dir/tail.bin 5000")
        == "verify-pattern: mismatch at 100: 0x0 != 0x76\n"
    )
    st_dir = stat_fields(m2, "/persist_dir")
    assert (st_dir["type"], st_dir["mode"], st_dir["nlink"]) == ("dir", "0755", "3"), st_dir
    assert stat_fields(m2, "/persist_dir/sub_new")["type"] == "dir"
    assert run(m2, "vfstest stat /persist_dir/sub_old") == "stat: NoEnt\n"
    assert run(m2, "vfstest readlink /persist_link").strip() == "/persist_dir/hello.txt"
    assert run(m2, "vfstest cat /persist_link").strip() == "persisted-ext4-v2"
    assert run(m2, "vfstest readlink /long_sym").strip() == long_target

    m2.send_line("exit")
    assert m2.wait_exit(10) == EXIT_SUCCESS
    m2.close()


BOOTING = r"INFO  boot: Lumen \d+\.\d+\.\d+ \(x86_64\) booting"
MOTD = "/etc/motd"


def write_unsynced(m, tag, disk, pristine):
    """Write on the ext4 root without `sync` (#352): a new directory, a file
    and a multi-page file in it, and `/etc/motd`, which the image ships,
    rewritten. Then the precondition that makes a later read-back mean
    something: the block cache still holds dirty blocks, and the disk image
    `disk` still has its `pristine` bytes (nothing written reached it)."""
    assert stat_fields(m, MOTD)["type"] == "file", "the image has no /etc/motd to rewrite"
    assert run(m, f"vfstest mkdir /unsynced_{tag} 0755") == "mkdir: ok\n"
    text = f"unsynced-{tag}"
    assert run(m, f"vfstest write /unsynced_{tag}/new.txt {text}") == f"wrote {len(text)}\n"
    assert run(m, f"vfstest write-pattern /unsynced_{tag}/big.bin 9001") == "wrote 9001\n"
    motd = f"motd-{tag}"
    assert run(m, f"vfstest write {MOTD} {motd}") == f"wrote {len(motd)}\n"
    assert blk_dirty(m) > 0, "nothing left to sync: the test would prove nothing"
    assert disk.read_bytes() == pristine, "the writes reached the disk before the power command"


def check_unsynced(m, tag):
    """What `write_unsynced(m, tag, ...)` wrote reads back, on the next boot
    (after `reboot` in the same machine, or after `shutdown` in a new one;
    all three read before comparing, so a failure shows each)."""
    got = (
        run(m, f"vfstest cat /unsynced_{tag}/new.txt"),
        run(m, f"vfstest verify-pattern /unsynced_{tag}/big.bin 9001"),
        run(m, f"vfstest cat {MOTD}"),
    )
    assert got == (f"unsynced-{tag}\n", "verify-pattern: ok (9001)\n", f"motd-{tag}\n")


def test_ext4_unsynced_writes_survive_reboot(boot, tmp_path):
    # `reboot` syncs every filesystem before it resets the machine (#352):
    # what was written without `sync` is on the disk when it boots again, in
    # the same QEMU on the same image, which passes e2fsck.
    disk = copy_rootfs(tmp_path)
    pristine = disk.read_bytes()
    m = shell(boot, reboot=True, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m.output)
    write_unsynced(m, "reboot", disk, pristine)
    m.qmp()  # connected before the reset, so it gets the RESET event
    start = len(m.output)
    m.send_line("reboot")
    m.expect(r"reboot: restarting\n", 60)
    assert m.wait_event("RESET", 120)["data"]["reason"] == "guest-reset"
    second = m.expect(BOOTING, 120)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    assert "sync failed" not in m.output[start:]
    assert_quiet_boot(m.output[second.start() :])
    check_unsynced(m, "reboot")
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()
    e2fsck_check(disk)


def test_ext4_unsynced_writes_survive_shutdown(boot, tmp_path):
    # `shutdown` syncs every filesystem before it powers off (#352): what was
    # written without `sync` is on the disk, which passes e2fsck, and a
    # second machine booted on it reads it back.
    disk = copy_rootfs(tmp_path)
    pristine = disk.read_bytes()
    m1 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m1.output)
    write_unsynced(m1, "shutdown", disk, pristine)
    m1.qmp()  # connected before the power-off, so it gets the SHUTDOWN event
    start = len(m1.output)
    m1.send_line("shutdown")
    m1.expect(r"shutdown: powering off\n", 60)
    assert m1.wait_event("SHUTDOWN", 120)["data"] == {"guest": True, "reason": "guest-shutdown"}
    assert m1.wait_exit(30) == 0
    assert "sync failed" not in m1.output[start:]
    m1.close()
    e2fsck_check(disk)

    m2 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m2.output)
    check_unsynced(m2, "shutdown")
    m2.send_line("exit")
    assert m2.wait_exit(10) == EXIT_SUCCESS
    m2.close()


def test_ext4_readonly_device_mounts_ro(boot, tmp_path):
    disk = copy_rootfs(tmp_path)
    before = disk.read_bytes()

    m = shell(boot, devices=Devices(blk_image=disk, blk_readonly=True, rng=True))
    assert_quiet_boot(m.output)

    assert mount_rows(m)["/"] == ("ext4", "0:1", "ro")
    assert run(m, "vfstest cat /etc/alpine-release").strip() == "3.21.10"
    assert "RoFs" in run(m, "vfstest write /ro_fail.txt blocked")
    # Syncing a read-only root writes nothing.
    assert run(m, "sync") == "sync: ok\n"

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()
    assert disk.read_bytes() == before


def test_dev_tmp_proc_filesystems(boot):
    # Boot without a disk: falls back to a tmpfs root (0:1), with no warning,
    # plus /tmp, /dev, /proc.
    m = shell(boot, devices=Devices(rng=True))
    assert_quiet_boot(m.output)

    out = run(m, "mount")
    assert out.startswith("mount: 4 mounts\n"), out  # the header counts the table's rows
    table = parse_mount(out)
    assert without_quota(table) == {
        "/": ("tmpfs", "0:1", "rw"),
        "/tmp": ("tmpfs", "0:2", "rw"),
        "/dev": ("devfs", "0:3", "rw"),
        "/proc": ("procfs", "0:4", "ro"),
    }
    assert run(m, "mount | count") == "4\n"
    assert "INFO  vfs: mounted / (tmpfs, rw) + /tmp /dev /proc\n" in m.output
    # Both tmpfs mounts show their quota (#324), the others none.
    for target in ("/", "/tmp"):
        used, limit = quota_in(table, target)
        assert limit == TMPFS_LIMIT and 0 <= used <= limit, (target, used, limit)
    assert (table["/dev"][3:], table["/proc"][3:]) == (("-", "-"), ("-", "-"))
    assert run(m, "mount | where limit > 0 | count") == "2\n"

    dev_ls = set(run(m, "vfstest ls /dev").splitlines())
    assert {"null", "zero", "random", "urandom", "tty", "console"}.issubset(dev_ls), dev_ls

    for proc_file in ("/proc/cpuinfo", "/proc/meminfo", "/proc/mounts", "/proc/version"):
        st = run(m, f"vfstest stat {proc_file}")
        assert "dev=0:4" in st and "size=0" in st, st
    assert "MemTotal:" in run(m, "vfstest cat /proc/meminfo")
    assert "processor" in run(m, "vfstest cat /proc/cpuinfo")
    proc_mounts = run(m, "vfstest cat /proc/mounts")
    # Linux's six fields, not `mount`'s quota columns.
    assert "devfs /dev devfs rw 0 0\n" in proc_mounts, proc_mounts
    assert "tmpfs /tmp tmpfs rw 0 0\n" in proc_mounts, proc_mounts
    assert "Linux version" in run(m, "vfstest cat /proc/version")

    out = run(m, "vfstest dev-tmp-proc")
    assert out.startswith("dev-tmp-proc: ok ("), out
    assert run(m, "vfstest read /dev/urandom 32") == "read: 32\n"
    # A length from the shell can't make the kernel allocate without bound.
    assert (
        run(m, "vfstest write-pattern /tmp/huge.bin 99999999999")
        == "write-pattern: length exceeds 16 MiB\n"
    )

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()


def test_tmpfs_quota_returns_after_ring3_files(boot, tmp_path):
    """#324: `/tmp`'s quota as `mount` shows it follows what ring 3 makes
    there. Alpine's busybox makes a directory with a copy of itself, a
    hard link to it, a symlink, an empty file, and a subdirectory holding
    another empty file, which raise the count by at least the copy's pages;
    `rm -r` of it all brings the count, and `/tmp`'s listing, back to
    exactly where they were."""
    disk = copy_rootfs(tmp_path)
    m = shell(boot, devices=Devices(blk_image=disk, rng=True))
    warm_up(m)

    def busybox(args):
        # Leaked frames aren't checked: what a run makes on /tmp (`cp`'s
        # pages, say) stays there, counted by the quota checked below.
        done = run_busybox(m, args)
        assert done.status == 0, done.raw

    listed_before = run(m, "vfstest ls /tmp")
    assert "." in listed_before.splitlines(), listed_before  # a listing, not an error
    used_before, limit = tmpfs_quota(m, "/tmp")
    assert limit == TMPFS_LIMIT
    busybox("mkdir /tmp/q324")
    busybox("cp /bin/busybox /tmp/q324/data")
    busybox("ln /tmp/q324/data /tmp/q324/hard")
    busybox("ln -s data /tmp/q324/soft")
    busybox("touch /tmp/q324/empty")
    busybox("mkdir /tmp/q324/sub")
    busybox("touch /tmp/q324/sub/inner")
    size = int(stat_fields(m, "/tmp/q324/data")["size"])
    assert size > 0
    pages = -(-size // 4096)
    used_with_files, _ = tmpfs_quota(m, "/tmp")
    assert used_with_files >= used_before + pages * 4096, (used_before, used_with_files, size)
    busybox("rm -r /tmp/q324")
    assert run(m, "vfstest ls /tmp") == listed_before
    assert tmpfs_quota(m, "/tmp") == (used_before, TMPFS_LIMIT)

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()


def test_dev_urandom_refuses_when_unseeded(boot):
    # Boot default machine (qemu64, no virtio-rng): RNG pool is never seeded.
    m = shell(boot)
    assert "INFO  rng: no randomness source has answered" in m.output

    assert run(m, "vfstest read /dev/random 32") == "read: Again\n"
    assert run(m, "vfstest read /dev/urandom 32") == "read: Again\n"
    assert run(m, "vfstest read /dev/urandom 0") == "read: 0\n"

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()


def test_ext4_deletions_and_renames_fsck_clean(boot, tmp_path):
    disk = copy_rootfs(tmp_path)
    m1 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m1.output)

    for cmd in (
        "vfstest mkdir /pa",
        "vfstest mkdir /pb",
        "vfstest mkdir /pa/sub",
        "vfstest mkdir /pa/x",
        "vfstest mkdir /pb/x",
        "vfstest mkdir /gonedir",
    ):
        assert run(m1, cmd) == "mkdir: ok\n", cmd
    assert run(m1, "vfstest write /f1 one") == "wrote 3\n"
    assert run(m1, "vfstest write /f2 two") == "wrote 3\n"
    assert run(m1, "vfstest write /gone.txt bye") == "wrote 3\n"
    # A directory across directories, a file over a file, a directory over an
    # empty directory; then an unlink and an rmdir.
    assert run(m1, "vfstest rename /pa/sub /pb/sub") == "rename: ok\n"
    assert run(m1, "vfstest rename /f1 /f2") == "rename: ok\n"
    assert run(m1, "vfstest rename /pa/x /pb/x") == "rename: ok\n"
    assert run(m1, "vfstest unlink /gone.txt") == "unlink: ok\n"
    assert run(m1, "vfstest rmdir /gonedir") == "rmdir: ok\n"
    assert run(m1, "sync") == "sync: ok\n"
    m1.close()

    e2fsck_check(disk)

    m2 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m2.output)
    assert stat_fields(m2, "/pa")["nlink"] == "2"
    assert stat_fields(m2, "/pb")["nlink"] == "4"
    assert stat_fields(m2, "/pb/sub")["nlink"] == "2"
    assert stat_fields(m2, "/pb/x")["type"] == "dir"
    assert run(m2, "vfstest cat /f2") == "one\n"
    for gone in ("/f1", "/gone.txt", "/gonedir", "/pa/sub", "/pa/x"):
        assert run(m2, f"vfstest stat {gone}") == "stat: NoEnt\n", gone
    m2.send_line("exit")
    assert m2.wait_exit(10) == EXIT_SUCCESS
    m2.close()


# kernel/src/vfs/testdata/unsigned-hash.ext4 (tools/ext4-unsigned-hash-image.py):
# an htree directory `/d` of 80 ASCII names and three with bytes of 0x80 or
# above, `é-utf8` among them, indexed by the unsigned half-MD4 hash.
UNSIGNED_HASH_IMAGE = (
    Path(__file__).resolve().parents[2]
    / "kernel"
    / "src"
    / "vfs"
    / "testdata"
    / "unsigned-hash.ext4"
)
# Where the superblock's `s_flags` is in the image.
S_FLAGS_AT = 1024 + 0x160


def htree_volume(dest: Path, alg: str, s_flags: int) -> Path:
    """A copy of UNSIGNED_HASH_IMAGE with `s_flags` and its default hash set,
    `/d` re-indexed by `e2fsck -fyD` with the hash they say (Linux's own,
    e2fsprogs' `ext2fs_dirhash`), checked clean, and both read back from the
    image: `s_flags` and `/d`'s root block's hash version."""
    image = dest / f"{alg}-{s_flags}.ext4"
    shutil.copyfile(UNSIGNED_HASH_IMAGE, image)
    _, e2fsck = find_e2fsprogs()
    tool = e2fsck.parent / "debugfs"
    for request in (f"ssv flags {s_flags}", f"ssv def_hash_version {alg}"):
        proc = subprocess.run(
            [str(tool), "-w", "-R", request, str(image)],
            capture_output=True,
            text=True,
            check=False,
        )
        errors = [line for line in proc.stderr.splitlines()[1:] if line]  # past the banner
        assert proc.returncode == 0 and not errors, (request, proc.stderr)
    proc = subprocess.run([str(e2fsck), "-fyD", str(image)], capture_output=True, check=False)
    assert proc.returncode in (0, 1), proc
    e2fsck_check(image)
    data = image.read_bytes()
    assert data[S_FLAGS_AT] == s_flags
    first = int(debugfs(image, "blocks d").split()[0])
    assert data[first * 1024 + 0x1C] == {"half_md4": 1, "tea": 2}[alg]
    return image


def dir_size(image: Path, path: str) -> int:
    """`path`'s size on `image`, as `debugfs stat` reads it."""
    found = re.search(r"\bSize: (\d+)", debugfs(image, f"stat {path}"))
    assert found, path
    return int(found.group(1))


@pytest.mark.parametrize(
    ("alg", "s_flags"),
    [("half_md4", 2), ("tea", 2), ("half_md4", 1), ("tea", 1)],
    ids=["half_md4-unsigned", "tea-unsigned", "half_md4-signed", "tea-signed"],
)
def test_ext4_htree_names_with_high_bytes_fsck_clean(boot, tmp_path, alg, s_flags):
    """#313: on an ext4 root whose htree directory `/d` is indexed by either
    hash (half-MD4, TEA), signed or unsigned as `s_flags` says (an unsigned
    one mounting writable, no longer read-only as #308's guard had it), a
    name with bytes of 0x80 or above is looked up, created, renamed into the
    directory (enough of them to split a leaf), made a directory, unlinked
    and removed, and the volume then passes `e2fsck -fn`, which checks each
    entry's hash against the leaf it sits in, as Linux computes it."""
    disk = htree_volume(tmp_path, alg, s_flags)
    size = dir_size(disk, "d")
    m = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m.output)
    assert mount_rows(m)["/"] == ("ext4", "0:1", "rw")
    assert run(m, "vfstest cat /d/é-utf8") == "x\n"
    assert stat_fields(m, "/d/é-utf8")["type"] == "file"

    # 63 characters, 183 bytes each: eight of them overflow every leaf's
    # free space put together.
    def long(n):
        return "€" * 60 + f"-{n:02}"

    for n in range(8):
        assert run(m, f"vfstest rename /d/entry-number-{n:02} /d/{long(n)}") == "rename: ok\n"
    assert run(m, f"vfstest write /d/{long(8)} eight") == "wrote 5\n"
    assert run(m, "vfstest mkdir /d/é-dir") == "mkdir: ok\n"
    assert run(m, "vfstest write /d/é-dir/ü inner") == "wrote 5\n"
    for n in range(8):
        assert run(m, f"vfstest cat /d/{long(n)}") == f"{n:02}\n", n
        assert run(m, f"vfstest stat /d/entry-number-{n:02}") == "stat: NoEnt\n", n
    assert run(m, f"vfstest cat /d/{long(8)}") == "eight\n"
    assert run(m, "vfstest cat /d/é-utf8") == "x\n"
    assert run(m, "vfstest cat /d/é-dir/ü") == "inner\n"
    assert run(m, "vfstest unlink /d/é-utf8") == "unlink: ok\n"
    assert run(m, "vfstest stat /d/é-utf8") == "stat: NoEnt\n"
    assert run(m, f"vfstest unlink /d/{long(3)}") == "unlink: ok\n"
    assert run(m, "vfstest mkdir /d/ö-gone") == "mkdir: ok\n"
    assert run(m, "vfstest rmdir /d/ö-gone") == "rmdir: ok\n"
    assert run(m, "sync") == "sync: ok\n"
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()

    e2fsck_check(disk)
    # The directory grew a leaf (read from the disk: the adapter doesn't
    # refresh a directory's cached size as ext4plus grows it).
    grew = dir_size(disk, "d")
    assert grew > size, f"/d didn't split a leaf: {size} -> {grew} bytes"

    # A second boot finds them from the disk, by their hashes.
    m2 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m2.output)
    for n in (0, 1, 2, 4, 5, 6, 7):
        assert run(m2, f"vfstest cat /d/{long(n)}") == f"{n:02}\n", n
    assert run(m2, f"vfstest cat /d/{long(8)}") == "eight\n"
    assert run(m2, "vfstest cat /d/é-dir/ü") == "inner\n"
    for gone in ("é-utf8", long(3), "ö-gone"):
        assert run(m2, f"vfstest stat /d/{gone}") == "stat: NoEnt\n", gone
    m2.send_line("exit")
    assert m2.wait_exit(10) == EXIT_SUCCESS
    m2.close()


def test_unmountable_ext4_disk_warns_and_falls_back(boot, tmp_path):
    # An ext4 superblock (the magic is there) whose inodes-per-group field is
    # 0: the disk can't be mounted, which must be said, not hidden.
    disk = copy_rootfs(tmp_path)
    with disk.open("r+b") as f:
        f.seek(1024 + 0x28)
        f.write(b"\0\0\0\0")
    m = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert re.search(
        r"WARN  vfs: blk\d+: has an ext4 superblock but won't mount: Inval\n", m.output
    ), m.output
    assert mount_rows(m)["/"] == ("tmpfs", "0:1", "rw")
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()


# tools/linux-m7/m7_vfs.rs (built into build/linux/, so tools/mkrootfs.sh
# puts it in the rootfs's /usr/local/bin): it prints `ok <check>` for each
# check passed, and `FAIL <check>: ...` for the first that isn't.
M7_VFS = "/usr/local/bin/m7_vfs"
EXIT_LINE = rf"linux: {M7_VFS} exit=(\d+) leaked_frames=(-?\d+)"


def run_m7_vfs(m, args, oks, timeout=60):
    """Run `m7_vfs <args>` from the serial shell: it prints exactly `oks`
    (each line `ok <oks[i]>`), in order, and exits 0; its leaked_frames."""
    out = run(m, f"linux {M7_VFS} {args}", timeout)
    lines = out.splitlines()
    ended = re.fullmatch(EXIT_LINE, lines[-1]) if lines else None
    assert ended, out
    assert lines[:-1] == [f"ok {c}" for c in oks], out
    assert ended[1] == "0", out
    return int(ended[2])


def m7_vfs(m, args, checks):
    """`run_m7_vfs` twice: the second run leaks no frame. Every file it
    writes it removes, so the page cache keeps none of their pages; the
    first run may grow caches the kernel keeps (the block cache's slots,
    the heap), so its count isn't checked."""
    run_m7_vfs(m, args, checks)
    assert run_m7_vfs(m, args, checks) == 0


POSIX_CHECKS = [
    "setup",
    "symlinks-and-eloop",
    "trailing-slash-nofollow",
    "o-path",
    "hard-links",
    "unlink-while-open",
    "rmdir-while-open",
    "getdents64",
    "non-utf8-names",
    "pread64-pwrite64",
    "stat-lstat-newfstatat-statx",
    "chmod-chown-umask-utimensat",
    "chdir-fchdir-getcwd",
    "exdev-and-enotty",
    "cleanup",
]


KEEP_CHECKS = [
    "setup",
    "hard-link-and-symlinks",
    "metadata",
    "exchanged-file-and-directory",
    "moved-directory",
    "non-utf8-names",
]


def test_posix_vfs_semantics(boot, tmp_path):
    """#254's POSIX file syscalls from ring 3, by a dynamic musl program on
    the Alpine rootfs (`m7_vfs posix`, each check's details in
    tools/linux-m7/m7_vfs.rs), run each way round, twice each (the second
    run leaking no frame): in ext4's `/m7v` (with `/tmp/m7v` as the other
    filesystem) and in tmpfs's `/tmp/m7s` (with `/m7s`), so each filesystem
    gets every check: symlinks and ELOOP (a loop, 40 links followed and 41
    not, O_NOFOLLOW), a trailing slash following a final symlink under
    O_NOFOLLOW, O_PATH descriptors, hard links and linkat's
    AT_SYMLINK_FOLLOW, unlink and rmdir while open (#318: `..` from the
    removed directory as cwd is its parent: chdir, getcwd after it, open
    O_DIRECTORY, and O_CREAT's EISDIR), getdents64 (types,
    d_ino, a small buffer's cursor, a moved directory's ".."), names that
    aren't UTF-8 (#268: a file, a directory and a symlink target made,
    listed, renamed, hard-linked, read back by getcwd and /proc/self/fd, and
    removed),
    pread64/pwrite64 leaving the offset alone, stat/lstat/newfstatat/statx
    agreeing, chmod/chown/umask/utimensat (UTIME_NOW, UTIME_OMIT, a NULL
    path, a link), chdir/fchdir/getcwd (through a link, a renamed and a
    removed cwd), EXDEV between the two filesystems and ENOTTY from a tty
    ioctl on a file. It removes what it made. Then `m7_vfs keep` leaves a
    small tree on ext4 (a hard link, a short and a long symlink, two files'
    mode, owner and times, one set before an in-program `sync` and one
    after it, an exchanged file and directory, a directory moved to another
    parent, a file and a symlink whose names and target aren't UTF-8, which
    `vfstest ls` shows escaped): the disk passes e2fsck with it there, and a
    second boot on the disk reads it all back (`m7_vfs keep-check`)."""
    disk = copy_rootfs(tmp_path)
    m = shell(boot, devices=Devices(blk_image=disk, rng=True))
    m7_vfs(m, "posix /m7v /tmp/m7v", POSIX_CHECKS)
    m7_vfs(m, "posix /tmp/m7s /m7s", POSIX_CHECKS)
    for gone in ("/m7v", "/m7s", "/tmp/m7v", "/tmp/m7s"):
        assert run(m, f"vfstest stat {gone}") == "stat: NoEnt\n", gone

    # What it keeps stays in the page cache: its frame count isn't checked.
    run_m7_vfs(m, "keep /m7k", KEEP_CHECKS)
    assert run(m, "sync") == "sync: ok\n"
    assert blk_dirty(m) == 0
    f, hl = stat_fields(m, "/m7k/f"), stat_fields(m, "/m7k/hl")
    assert (f["ino"], f["nlink"], f["mode"]) == (hl["ino"], "2", "0640"), (f, hl)
    assert stat_fields(m, "/m7k/xf")["type"] == "dir"
    assert stat_fields(m, "/m7k/xd")["type"] == "file"
    assert (stat_fields(m, "/m7k/p1")["nlink"], stat_fields(m, "/m7k/p2")["nlink"]) == ("2", "3")
    assert run(m, "vfstest readlink /m7k/sl").strip() == "f"
    # `vfstest ls` shows the names that aren't UTF-8 escaped, byte by byte.
    listed = run(m, "vfstest ls /m7k").splitlines()
    assert "\\xff\\xfe-kept" in listed and "\\xc3(-sl" in listed, listed
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()
    e2fsck_check(disk)

    # A second boot reads the tree from the disk, not the first one's cache.
    m2 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    run_m7_vfs(m2, "keep-check /m7k", KEEP_CHECKS[1:])
    assert stat_fields(m2, "/m7k/f")["mtime"] == "2000000.000000000"
    m2.send_line("exit")
    assert m2.wait_exit(10) == EXIT_SUCCESS
    m2.close()


LOCKS_CHECKS = [
    "setup",
    "flock-open-file-descriptions",
    "renameat2-noreplace",
    "renameat2-exchange",
    "rename-over-an-open-file",
    "cleanup",
]


def test_flock_and_atomic_rename(boot, tmp_path):
    """#254's advisory locks and renames from ring 3. `m7_vfs locks`, on
    ext4 and on tmpfs: flock between two open file descriptions of one
    file in one process (EWOULDBLOCK, a dup sharing its description's lock,
    released by LOCK_UN and by the description's last close, not a dup's),
    renameat2's RENAME_NOREPLACE (EEXIST) and RENAME_EXCHANGE, and rename
    over an open file (the descriptor keeps reading the old data, the name
    the new). Then fcntl record locks between two processes:
    `m7_vfs lock-hold`, typed on virtual terminal 2, takes a step per key
    (a write lock on [0,100) and a read lock on [200,210), unlocking
    [40,60), locking it again, closing another descriptor of the file it
    opened, locking [0,100) again, exiting), and after each
    `m7_vfs lock-probe`, run from serial, checks F_GETLK's answer (type,
    range, the holder's pid) and that F_SETLK conflicts (EAGAIN): the split
    into two ranges, their coalescing into one, the release by closing any
    descriptor, and by the exit, with no warning or error logged
    meanwhile. RENAME_NOREPLACE's EEXIST also comes onto a hard link of the
    same file and for a file onto a directory. (Two processes at once: their
    `leaked_frames` count each other's frames, so only the single-process
    runs' are checked.)"""
    disk = copy_rootfs(tmp_path)
    m = shell(boot, devices=Devices(blk_image=disk, rng=True))
    for d in ("/m7lk", "/tmp/m7lk"):
        m7_vfs(m, f"locks {d}", LOCKS_CHECKS)

    confirmed_keys(m, ["alt", "f2"])
    expect_screen(m, r"\Alumen>\s*\Z")
    holder_start = len(m.output)
    type_confirmed(m, f"linux {M7_VFS} lock-hold /m7.lock\n")
    pid = int(expect_screen(m, r"^hold: pid=(\d+) ready$", 30)[1])

    def step(name, *probes):
        # A key (confirmed reaching the keyboard's queue), then the holder's
        # line for the step it took, then the probes.
        confirmed_keys(m, "n")
        expect_screen(m, rf"^hold: {name}$", 20)
        run_m7_vfs(
            m, f"lock-probe /m7.lock {pid} {' '.join(probes)}", [f"probe {p}" for p in probes]
        )

    # `<t><start>+<len>=<expect>`: F_GETLK for a `<t>` (r/w) lock on that
    # range (`+0`: to the end) sees `unlck` or the holder's lock.
    step(
        "locked",
        "w0+0=w0+100",
        "r50+1=w0+100",
        "w100+100=unlck",
        "r200+10=unlck",
        "w150+100=r200+10",
    )
    step("split", "w0+0=w0+40", "w40+20=unlck", "r50+100=w60+40", "w45+10=unlck")
    step("coalesced", "w0+0=w0+100", "w45+10=w0+100", "w99+1=w0+100")
    step("released", "w0+0=unlck", "w0+300=unlck")
    step("relocked", "w0+0=w0+100")
    # It exits holding [0,100), which its exit gives up.
    confirmed_keys(m, "n")
    expect_screen(m, rf"^hold: done\nlinux: {M7_VFS} exit=0 leaked_frames=-?\d+$", 20)
    run_m7_vfs(m, f"lock-probe /m7.lock {pid} w0+0=unlck", ["probe w0+0=unlck"])
    # No warning or error while the holder ran: the steps that matched on
    # its terminal (`expect_screen`) don't look at serial's log lines.
    assert not unexpected_warnings(m.output[holder_start:]), m.output[holder_start:]
    assert run(m, "vfstest unlink /m7.lock") == "unlink: ok\n"

    assert run(m, "sync") == "sync: ok\n"
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()
    e2fsck_check(disk)
