"""Unmodified Alpine Linux software on Lumen's writable ext4 rootfs (M7, #255).

Each applet of Alpine's dynamic `/bin/busybox` runs as its own `linux
/bin/busybox <applet> ...` line: no ring-3 process can fork or exec yet, so
neither the Lumen shell nor ash runs pipelines or external commands (M8)."""

import re

from lumen_harness import BUILD_DIR
from lumen_harness.crash import BEGIN as CRASH_BEGIN
from lumen_harness.klog import unexpected_warnings
from lumen_harness.leaks import Leaks
from lumen_harness.qemu import Devices
from lumen_harness.rootfs import copy_rootfs, e2fsck_check
from lumen_harness.shell import PROMPT, run

BOOT_TIMEOUT = 60
MINIROOTFS = BUILD_DIR / "deps" / "alpine-minirootfs"
EXIT_LINE = re.compile(r"linux: /bin/busybox exit=(\d+) leaked_frames=-?\d+")
ASH_PROMPT = r"[~/][^\n]* # "


class Busybox:
    """Runs `/bin/busybox` applets, checking each one's leaked frames
    (`lumen_harness.leaks`): the page-cache pages of the files it creates
    (or frees)."""

    def __init__(self, m):
        self.m = m
        self.leaks = Leaks()

    def __call__(self, args, *, status=0, leaked=0):
        """`/bin/busybox <args>`'s output, less the `linux:` line, which must
        give `status` as its exit code and `leaked` frames."""
        out = run(self.m, f"linux /bin/busybox {args}", 60).replace("\r\n", "\n")
        lines = out.splitlines()
        assert lines, out
        done = EXIT_LINE.fullmatch(lines[-1])
        assert done and int(done.group(1)) == status, out
        self.leaks.check(lines[-1], leaked)
        return "".join(line + "\n" for line in lines[:-1])


def test_alpine_minirootfs_coreutils(boot, tmp_path):
    """Alpine's minirootfs (`build/alpine-rootfs.ext4`, copied for this
    test) boots as `/`, and its dynamic `/bin/busybox` runs through
    `/lib/ld-musl-x86_64.so.1`:

    - `cat` and `ls` read the files as the host has them;
    - `cp` copies a file to tmpfs; `mv` moves it from `/tmp` to `/` and
      back (across mounts: rename's EXDEV, then busybox copies and
      unlinks), and a directory tree the same way, the source gone each
      time and `cmp` finding the copy equal;
    - `grep -n`, `sed -n` with `p`, `sed -i` (a temporary file renamed over
      an ext4 one) and `awk` (a field filter, `END`'s `NR`) give what the
      same tools give on the host's copy of the files;
    - uncompressed `tar -cf` onto tmpfs, `-tf` listing it, and `-xf` onto
      ext4, the extracted files equal to the originals;
    - `ash -s` runs a script of built-ins fed line by line on stdin over
      serial (arithmetic, `cd` and `$PWD`, `for`, `[`, `case`, `read`), and
      its `exit 3` is the process's exit code.

    Each run leaks no frames but the pages of the files it makes (and,
    now and then, a page table for the kernel stack window:
    `lumen_harness.leaks`). Then `sync`, and
    the image passes the host's `e2fsck -fn`."""
    passwd = (MINIROOTFS / "etc" / "passwd").read_text()
    group = (MINIROOTFS / "etc" / "group").read_text()
    release = (MINIROOTFS / "etc" / "alpine-release").read_text()

    disk = copy_rootfs(tmp_path)
    m = boot("qemutest", devices=Devices(blk_image=disk, rng=True))
    m.expect(PROMPT, BOOT_TIMEOUT)
    busybox = Busybox(m)
    # The files the applets read in the page cache first, and ash's history
    # file made (it appends to it): no run counts their pages.
    for path in ("/etc/passwd", "/etc/group", "/etc/alpine-release"):
        assert re.fullmatch(r"read: \d+\n", run(m, f"vfstest read {path} 8192"))
    assert run(m, "vfstest write /.ash_history x") == "wrote 1\n"

    # ls and cat.
    assert busybox("cat /etc/alpine-release") == release
    assert busybox("cat /etc/passwd") == passwd
    top = busybox("ls -1 --color=never /").split()
    assert {"bin", "dev", "etc", "lib", "proc", "tmp", "usr"} <= set(top), top
    listing = busybox("ls -ln --color=never /etc/passwd")
    assert re.fullmatch(rf"-rw-r--r-- +1 +0 +0 +{len(passwd)} .* /etc/passwd\n", listing), listing

    # cp, then mv across the mounts both ways: a file, then a tree. A copy
    # holds a page (`leaked=1`); a move frees the source's as it fills the
    # destination's.
    assert busybox("cp /etc/passwd /tmp/passwd", leaked=1) == ""
    assert busybox("cmp /etc/passwd /tmp/passwd") == ""
    assert busybox("mv /tmp/passwd /passwd.moved") == ""
    assert busybox("cat /passwd.moved") == passwd
    gone = busybox("ls /tmp/passwd", status=1)
    assert "No such file or directory" in gone, gone
    assert busybox("mv /passwd.moved /tmp/passwd.back") == ""
    assert busybox("cmp /etc/passwd /tmp/passwd.back") == ""
    gone = busybox("ls /passwd.moved", status=1)
    assert "No such file or directory" in gone, gone

    assert busybox("mkdir -p /tmp/tree/sub") == ""
    assert busybox("cp /etc/group /tmp/tree/sub/group", leaked=1) == ""
    assert busybox("mv /tmp/tree /tree") == ""
    assert busybox("cmp /etc/group /tree/sub/group") == ""
    assert busybox("ls /tmp/tree", status=1) != ""
    assert busybox("mv /tree /tmp/tree2") == ""
    assert busybox("cmp /etc/group /tmp/tree2/sub/group") == ""
    assert busybox("ls /tree", status=1) != ""

    # grep, sed, awk, against the host's copies of the files.
    lines = passwd.splitlines()
    roots = "".join(f"{n}:{line}\n" for n, line in enumerate(lines, 1) if "root" in line)
    assert busybox("grep -n root /etc/passwd") == roots
    nologin = sum(1 for line in lines if line.endswith("/sbin/nologin"))
    assert busybox("grep -c /sbin/nologin$ /etc/passwd") == f"{nologin}\n"
    root_line = next(line for line in lines if line.startswith("root:"))
    assert busybox("sed -n s/^root:/ROOT:/p /etc/passwd") == "ROOT:" + root_line[5:] + "\n"
    assert busybox("cp /etc/group /group.sed", leaked=1) == ""
    assert busybox("sed -i s/^root:/toor:/ /group.sed") == ""
    edited = "".join(
        ("toor:" + line[5:] if line.startswith("root:") else line) + "\n"
        for line in group.splitlines()
    )
    assert busybox("cat /group.sed") == edited
    uid0 = "".join(f"{f[0]}|{f[6]}\n" for f in (line.split(":") for line in lines) if f[2] == "0")
    assert busybox("awk -F: '$3 == 0 { print $1 \"|\" $7 }' /etc/passwd") == uid0
    assert busybox("awk 'END { print NR }' /etc/passwd") == f"{len(lines)}\n"

    # tar: created on tmpfs (one page: two headers, two files, the end),
    # extracted onto ext4 (a page per file).
    assert busybox("tar -cf /tmp/etc.tar -C /etc passwd group", leaked=1) == ""
    assert busybox("tar -tf /tmp/etc.tar") == "passwd\ngroup\n"
    assert busybox("mkdir /untar") == ""
    assert busybox("tar -xf /tmp/etc.tar -C /untar", leaked=2) == ""
    assert busybox("cmp /etc/passwd /untar/passwd") == ""
    assert busybox("cmp /etc/group /untar/group") == ""

    # ash -s: built-ins only, the script fed a line at a time. Each echo
    # prints what its input line doesn't spell out, so its echo can't match.
    ash_from = len(m.output)
    m.send_line("linux /bin/busybox ash -s")
    m.expect(ASH_PROMPT, 30)
    script = [
        ('echo "sum=$((6 * 7))"', r"^sum=42$"),
        ('cd /tmp && echo "cwd=$PWD"', r"^cwd=/tmp$"),
        ('for i in 1 2 3; do echo "loop-$i"; done', r"^loop-1\nloop-2\nloop-3$"),
        ('R=release; [ -f /etc/alpine-$R ] && echo "found-$R"', r"^found-release$"),
        ('case $PWD in /tmp) echo "case-$PWD";; esac', r"^case-/tmp$"),
    ]
    for line, expected in script:
        m.send_line(line)
        m.expect(re.compile(expected, re.MULTILINE), 20)
        m.expect(ASH_PROMPT, 20)
    m.send_line("read NAME")
    m.send_line("lumen-input")
    m.expect(ASH_PROMPT, 20)
    m.send_line('echo "read=$NAME"')
    m.expect(re.compile(r"^read=lumen-input$", re.MULTILINE), 20)
    m.expect(ASH_PROMPT, 20)
    m.send_line("exit 3")
    busybox.leaks.check(m.expect(r"linux: /bin/busybox exit=3 leaked_frames=-?\d+", 30).group(0), 0)
    m.expect(PROMPT, 10)
    assert CRASH_BEGIN not in m.output
    assert unexpected_warnings(m.output[ash_from:]) == []

    assert run(m, "sync", 60) == "sync: ok\n"
    m.close()
    e2fsck_check(disk)
