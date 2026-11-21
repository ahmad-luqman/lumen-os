"""POSIX VFS, PageCache, writable ext4 rootfs, tmpfs, devfs, and procfs scenarios."""

import re

from lumen_harness.calibration import assert_quiet_boot
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import copy_rootfs, e2fsck_check
from lumen_harness.shell import PROMPT, run

SHELLS = r"INFO  shell: shells? on serial.*; type 'help'\n"


def shell(boot, **kwargs):
    m = boot("qemutest", cmdline="lumen.qemu_exit", **kwargs)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def mount_rows(m):
    """`mount`'s table as {target: (fs, dev, mode)}."""
    rows = {}
    for line in run(m, "mount").splitlines():
        cells = line.split()
        if len(cells) == 4 and cells[0].startswith("/"):
            rows[cells[0]] = tuple(cells[1:])
    return rows


def stat_fields(m, path):
    """`vfstest stat`'s `key=value` fields."""
    out = run(m, f"vfstest stat {path}")
    fields = dict(f.split("=", 1) for f in out.split() if "=" in f)
    assert "ino" in fields, out
    return fields


def test_ext4_read_write_persists_across_reboot(boot, tmp_path):
    disk = copy_rootfs(tmp_path)

    # Boot 1: attach Alpine ext4 rootfs RW, verify mount table, existing files,
    # write files/dirs/symlinks/chmod/truncate, sync, then abruptly kill QEMU.
    m1 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m1.output)

    assert mount_rows(m1) == {
        "/": ("ext4", "0:1", "rw"),
        "/tmp": ("tmpfs", "0:2", "rw"),
        "/dev": ("devfs", "0:3", "ro"),
        "/proc": ("procfs", "0:4", "ro"),
    }
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
    assert run(m1, "blk | select dirty").strip().splitlines()[-1].strip() == "0"
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
    # Boot without a disk: silently falls back to tmpfs root (0:1) + /tmp, /dev, /proc.
    m = shell(boot, devices=Devices(rng=True))
    assert_quiet_boot(m.output)

    assert mount_rows(m) == {
        "/": ("tmpfs", "0:1", "rw"),
        "/tmp": ("tmpfs", "0:2", "rw"),
        "/dev": ("devfs", "0:3", "ro"),
        "/proc": ("procfs", "0:4", "ro"),
    }
    assert run(m, "mount | count") == "4\n"
    assert "INFO  vfs: mounted / (tmpfs, rw) + /tmp /dev /proc\n" in m.output

    dev_ls = set(run(m, "vfstest ls /dev").splitlines())
    assert {"null", "zero", "random", "urandom", "tty", "console"}.issubset(dev_ls), dev_ls

    for proc_file in ("/proc/cpuinfo", "/proc/meminfo", "/proc/mounts", "/proc/version"):
        st = run(m, f"vfstest stat {proc_file}")
        assert "dev=0:4" in st and "size=0" in st, st
    assert "MemTotal:" in run(m, "vfstest cat /proc/meminfo")
    assert "processor" in run(m, "vfstest cat /proc/cpuinfo")
    assert "devfs /dev devfs ro 0 0" in run(m, "vfstest cat /proc/mounts")
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
