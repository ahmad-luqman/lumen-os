"""POSIX VFS, PageCache, writable ext4 rootfs, tmpfs, devfs, and procfs scenarios."""

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


def test_ext4_read_write_persists_across_reboot(boot, tmp_path):
    disk = copy_rootfs(tmp_path)

    # Boot 1: attach Alpine ext4 rootfs RW, verify mount table, existing files,
    # write files/dirs/symlinks/chmod/truncate, sync, then abruptly kill QEMU.
    m1 = shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert_quiet_boot(m1.output)

    mounts = run(m1, "mount")
    assert "/ " in mounts and "ext4" in mounts and "0:1" in mounts and "rw" in mounts, mounts
    assert "/tmp" in mounts and "tmpfs" in mounts and "0:2" in mounts, mounts
    assert "/dev" in mounts and "devfs" in mounts and "0:3" in mounts and "ro" in mounts, mounts
    assert "/proc" in mounts and "procfs" in mounts and "0:4" in mounts, mounts
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
    assert run(m1, "vfstest symlink /persist_link /persist_dir/hello.txt") == "symlink: ok\n"
    long_target = "/persist_dir/0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
    assert len(long_target) >= 60
    assert run(m1, f"vfstest symlink /long_sym {long_target}") == "symlink: ok\n"

    assert run(m1, "sync") == "sync: ok\n"
    assert run(m1, "blk | select dirty").strip().splitlines()[-1].strip() == "0"

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
    st_trunc = run(m2, "vfstest stat /persist_dir/trunc.bin")
    assert "size=123" in st_trunc, st_trunc
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

    mounts = run(m, "mount")
    assert "ext4" in mounts and "0:1" in mounts and "ro" in mounts, mounts
    assert run(m, "vfstest cat /etc/alpine-release").strip() == "3.21.10"
    assert "RoFs" in run(m, "vfstest write /ro_fail.txt blocked")

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()
    assert disk.read_bytes() == before


def test_dev_tmp_proc_filesystems(boot):
    # Boot without a disk: silently falls back to tmpfs root (0:1) + /tmp, /dev, /proc.
    m = shell(boot, devices=Devices(rng=True))
    assert_quiet_boot(m.output)

    mounts = run(m, "mount")
    assert "tmpfs" in mounts and "0:1" in mounts and "0:2" in mounts, mounts
    assert "devfs" in mounts and "0:3" in mounts, mounts
    assert "procfs" in mounts and "0:4" in mounts, mounts
    assert run(m, "mount | count") == "4\n"

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

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()
