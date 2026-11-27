"""Unmodified Alpine Linux software on Lumen's writable ext4 rootfs.

M7 checks individual applets and ash built-ins; M8's D1 gate runs ash
scripts, external commands, pipelines and background jobs (#393).
"""

import os
import re
import sqlite3
import sys

import pytest
from lumen_harness import BUILD_DIR
from lumen_harness.crash import BEGIN as CRASH_BEGIN
from lumen_harness.klog import unexpected_warnings
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import (
    BUSYBOX_EXIT,
    copy_dev_rootfs,
    copy_rootfs,
    debugfs,
    debugfs_names,
    e2fsck_check,
    run_busybox,
    warm_up,
)
from lumen_harness.shell import PROMPT, run

BOOT_TIMEOUT = 60
MINIROOTFS = BUILD_DIR / "deps" / "alpine-minirootfs"
ASH_PROMPT = r"[~/][^\n]* # "


class Busybox:
    """Runs `/bin/busybox` applets, each one's leaked frames exactly the
    page-cache pages of the files it creates (or frees)."""

    def __init__(self, m):
        self.m = m
        warm_up(m)

    def __call__(self, args, *, status=0, leaked=0):
        """`/bin/busybox <args>`'s output, less the `linux:` line, which must
        give `status` as its exit code and `leaked` frames."""
        done = run_busybox(self.m, args)
        assert done.status == status, done.raw
        assert done.leaked == leaked, f"expected leaked_frames={leaked}:\n{done.raw}"
        return done.output

    def assert_gone(self, path):
        """`ls <path>` fails: no such file or directory."""
        out = self("ls " + path, status=1)
        assert "No such file or directory" in out, out


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

    Each run leaks exactly the pages of the files it makes (or frees),
    every kernel stack's page tables included (#300). Then `sync` and
    `exit`; the image passes the host's `e2fsck -fn`, and its `debugfs`
    finds the changes on the disk: `/group.sed` edited, `/untar`'s files
    equal to the host's, `/` holding exactly the image's names plus the
    test's (the moved file and tree gone, no temporary file left), and
    `/tmp` (a tmpfs mount over it) empty there."""
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
    busybox.assert_gone("/tmp/passwd")
    assert busybox("mv /passwd.moved /tmp/passwd.back") == ""
    assert busybox("cmp /etc/passwd /tmp/passwd.back") == ""
    busybox.assert_gone("/passwd.moved")

    assert busybox("mkdir -p /tmp/tree/sub") == ""
    assert busybox("cp /etc/group /tmp/tree/sub/group", leaked=1) == ""
    assert busybox("mv /tmp/tree /tree") == ""
    assert busybox("cmp /etc/group /tree/sub/group") == ""
    busybox.assert_gone("/tmp/tree")
    assert busybox("mv /tree /tmp/tree2") == ""
    assert busybox("cmp /etc/group /tmp/tree2/sub/group") == ""
    busybox.assert_gone("/tree")

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

    # ash -s: built-ins only, the script fed a line at a time. Each `echo`
    # prints text its typed line doesn't contain, so the terminal's echo of
    # that line can't match the pattern.
    ash_from = len(m.output)
    m.send_line("linux /bin/busybox ash -s")
    m.expect(ASH_PROMPT, 30)
    script = [
        ('echo "sum=$((6 * 7))"', r"^sum=42\r?\n"),
        ('cd /tmp && echo "cwd=$PWD"', r"^cwd=/tmp\r?\n"),
        ('for i in 1 2 3; do echo "loop-$i"; done', r"^loop-1\r?\nloop-2\r?\nloop-3\r?\n"),
        ('R=release; [ -f /etc/alpine-$R ] && echo "found-$R"', r"^found-release\r?\n"),
        ('case $PWD in /tmp) echo "case-$PWD";; esac', r"^case-/tmp\r?\n"),
    ]
    for line, expected in script:
        m.send_line(line)
        m.expect(re.compile(expected, re.MULTILINE), 20)
        m.expect(ASH_PROMPT, 20)
    m.send_line("read NAME")
    m.send_line("lumen-input")
    m.expect(ASH_PROMPT, 20)
    m.send_line('echo "read=$NAME"')
    m.expect(re.compile(r"^read=lumen-input\r?\n", re.MULTILINE), 20)
    m.expect(ASH_PROMPT, 20)
    m.send_line("exit 3")
    m.expect(r"linux: /bin/busybox exit=3 leaked_frames=0\n", 30)
    m.expect(PROMPT, 10)
    assert CRASH_BEGIN not in m.output
    assert unexpected_warnings(m.output[ash_from:]) == []

    assert run(m, "sync", 60) == "sync: ok\n"
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    e2fsck_check(disk)

    pristine = debugfs_names(BUILD_DIR / "alpine-rootfs.ext4", "/")
    assert "tmp" in pristine, pristine
    assert debugfs_names(disk, "/") == pristine | {".ash_history", "group.sed", "untar"}
    assert debugfs_names(disk, "/tmp") == set()
    assert debugfs_names(disk, "/untar") == {"passwd", "group"}
    assert debugfs(disk, "cat /group.sed") == edited
    assert debugfs(disk, "cat /untar/passwd") == passwd
    assert debugfs(disk, "cat /untar/group") == group


def test_ash_scripts_and_pipelines(boot, tmp_path):
    """D1: real ash scripts on the developer image, with exact output and status.

    External commands carry more than a pipe's capacity through three stages;
    redirections, subshell isolation, command substitution, signal/EXIT traps
    and selected background waits preserve data, exit statuses and child reaping.
    FIFO handshakes establish readiness for selected waits. Bare wait returns
    zero and leaves both children reaped; this does not prove it blocked.
    The changed ext4 files survive sync and pass the host's fsck.
    """
    # The developer image is its own target (`make rootfs-dev`): `make test`
    # leaves it out (test_make_deps_build_and_test_leave_the_dev_image_out),
    # so a local run without it skips. CI builds it first, so there a missing
    # image fails rather than skips, and the gate can't pass unchecked.
    if not (BUILD_DIR / "alpine-dev.ext4").is_file():
        if os.environ.get("CI") and sys.platform == "linux":
            pytest.fail("build/alpine-dev.ext4 is missing: CI runs `make rootfs-dev` first")
        pytest.skip("no build/alpine-dev.ext4 (run `make rootfs-dev`)")
    disk = copy_dev_rootfs(tmp_path)
    script = tmp_path / "d1.sh"
    script.write_text(
        r"""set -eu
export PATH=/bin:/usr/bin
/bin/busybox mkdir /ash-d1
cd /ash-d1

# Explicit busybox paths force external execs, even for ash built-ins.
# 108894 bytes, larger than the default 65536-byte pipe, and EOF at each hop.
/bin/busybox seq 1 20000 | /bin/busybox cat | /bin/busybox awk '
    NR == 1 { first = $1 }
    NF != 1 || $1 != NR { exit 31 }
    { sum += $1; last = $1 }
    END { printf "pipeline=%d,%d,%d,%d\n", NR, first, last, sum }
'
/bin/busybox false | /bin/busybox true
left=$?
if /bin/busybox true | /bin/busybox false; then right=0; else right=$?; fi
printf 'pipeline-status=%s,%s\n' "$left" "$right"

printf 'discard this longer content\n' > data
printf 'alpha\n' > data
printf 'beta\n' >> data
/bin/busybox cat < data
{ printf 'stdout\n'; printf 'stderr\n' >&2; } > out 2> err
/bin/busybox cat out err
{ printf 'merged-out\n'; printf 'merged-err\n' >&2; } > merged 2>&1
/bin/busybox cat merged
# Order matters: stderr keeps the old stdout while stdout goes to a file.
{ printf 'file-only\n'; printf 'console-only\n' >&2; } 2>&1 > ordered
/bin/busybox cat ordered
printf 'stdout-restored\n'

value=parent
(cd /tmp; value=child; printf 'subshell=%s,%s\n' "$PWD" "$value")
printf 'parent=%s,%s\n' "$PWD" "$value"
captured=$(/bin/busybox printf 'first\nsecond\n\n')
[ "$captured" = 'first
second' ]
printf 'substitution=<%s>\n' "$captured"
if captured=$(/bin/busybox ash -c 'printf failed-value; exit 19'); then
    status=0
else
    status=$?
fi
printf 'substitution-status=%s,%s\n' "$status" "$captured"

# Execute a shell script, retaining argv and env.
cat > child.sh <<'CHILD'
#!/bin/sh
printf 'script=%s,%s,%s,%s\n' "$#" "$1" "$D1_VALUE" "$PWD"
exit 17
CHILD
/bin/busybox chmod 755 child.sh
export D1_VALUE=inherited
if ./child.sh 'two words'; then status=0; else status=$?; fi
printf 'script-status=%s\n' "$status"

# cat prints the source; ash's ENOEXEC fallback would execute its echo.
# This distinguishes the kernel's shebang handling from that fallback.
cat > cat.sh <<'CAT'
#!/bin/busybox cat
echo ash-fallback
CAT
/bin/busybox chmod 755 cat.sh
./cat.sh

trapped=missing
trap 'trapped=USR1' USR1
/bin/busybox kill -USR1 "$$"
printf 'trap=%s\n' "$trapped"
trap - USR1

assert_reaped() {
    if kill -0 "$1" 2>/dev/null; then
        printf 'not-reaped=%s\n' "$2"
        exit 31
    fi
}

# Both children announce readiness before the parent releases the second
# first. The first cannot finish until after wait has selected the second.
/bin/busybox mkfifo ready first-go second-go
(
    printf 'first\n' > ready
    read -r token < first-go
    [ "$token" = release-first ]
    printf 'first-done\n' > first-result
    exit 7
) &
first=$!
read -r ready_name < ready
[ "$ready_name" = first ]
kill -0 "$first"
(
    printf 'second\n' > ready
    read -r token < second-go
    [ "$token" = release-second ]
    printf 'second-done\n' > second-result
    exit 9
) &
second=$!
read -r ready_name < ready
[ "$ready_name" = second ]
[ "$first" != "$second" ]
kill -0 "$second"
printf 'release-second\n' > second-go
if wait "$second"; then status=0; else status=$?; fi
printf 'wait-second=%s\n' "$status"
assert_reaped "$second" selected-second
[ ! -e first-result ]
/bin/busybox cat second-result
printf 'release-first\n' > first-go
if wait "$first"; then status=0; else status=$?; fi
printf 'wait-first=%s\n' "$status"
assert_reaped "$first" selected-first
/bin/busybox cat first-result

# Bare wait returns zero despite distinct non-zero child statuses, and leaves
# both children reaped. This does not prove it blocked while they were alive:
# ash can incidentally reap finished children before the kill -0 checks.
(printf 'all-a\n' > all-a; exit 3) &
all_a=$!
(printf 'all-b\n' > all-b; exit 5) &
all_b=$!
if wait; then status=0; else status=$?; fi
printf 'wait-all=%s\n' "$status"
assert_reaped "$all_a" wait-all-a
assert_reaped "$all_b" wait-all-b
/bin/busybox cat all-a all-b
/bin/busybox rm ready first-go second-go
trap 'printf "exit-trap=%s\n" "$?"' EXIT
exit 23
""",
        encoding="utf-8",
    )
    # Stage only this test's disk, before QEMU opens it. The script never
    # travels through the console, so echoed source cannot satisfy a check.
    debugfs(disk, f'write "{script}" /d1.sh', write=True)
    m = boot("qemutest", memory="512M", devices=Devices(blk_image=disk, rng=True))
    m.expect(PROMPT, BOOT_TIMEOUT)
    done = run_busybox(m, "ash /d1.sh")
    assert done.status == 23, done.raw
    assert done.output.splitlines() == [
        "pipeline=20000,1,20000,200010000",
        "pipeline-status=0,1",
        "alpha",
        "beta",
        "stdout",
        "stderr",
        "merged-out",
        "merged-err",
        "console-only",
        "file-only",
        "stdout-restored",
        "subshell=/tmp,child",
        "parent=/ash-d1,parent",
        "substitution=<first",
        "second>",
        "substitution-status=19,failed-value",
        "script=1,two words,inherited,/ash-d1",
        "script-status=17",
        "#!/bin/busybox cat",
        "echo ash-fallback",
        "trap=USR1",
        "wait-second=9",
        "second-done",
        "wait-first=7",
        "first-done",
        "wait-all=0",
        "all-a",
        "all-b",
        "exit-trap=23",
    ], done.raw
    assert CRASH_BEGIN not in m.output
    assert run(m, "sync", 60) == "sync: ok\n"
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    e2fsck_check(disk)
    for name, contents in {
        "data": "alpha\nbeta\n",
        "out": "stdout\n",
        "err": "stderr\n",
        "merged": "merged-out\nmerged-err\n",
        "ordered": "file-only\n",
        "first-result": "first-done\n",
        "second-result": "second-done\n",
        "all-a": "all-a\n",
        "all-b": "all-b\n",
    }.items():
        assert debugfs(disk, f"cat /ash-d1/{name}") == contents, name
    assert debugfs_names(disk, "/ash-d1") == {
        "data",
        "out",
        "err",
        "merged",
        "ordered",
        "child.sh",
        "cat.sh",
        "first-result",
        "second-result",
        "all-a",
        "all-b",
    }


def _require_dev_rootfs() -> None:
    """Skip locally when `build/alpine-dev.ext4` hasn't been built; fail on Linux CI."""
    if not (BUILD_DIR / "alpine-dev.ext4").is_file():
        if os.environ.get("CI") and sys.platform == "linux":
            pytest.fail("build/alpine-dev.ext4 is missing: CI runs `make rootfs-dev` first")
        pytest.skip("no build/alpine-dev.ext4 (run `make rootfs-dev`)")


def test_gcc_gpp_compile_cpp_and_shared_lib(boot, tmp_path):
    """D2: unmodified Alpine gcc/g++ and GNU make on the developer image at 512 MiB.

    `make -j1` compiles a C helper object with `gcc -O2`, a C++ shared library
    (`libthrow.so`) with `g++ -O2 -fPIC -shared`, and a C++ executable linked
    against both with `-Wl,-rpath,.`; running it verifies normal calls and a
    C++ `std::runtime_error` thrown and unwound with RAII destructors across
    the `.so` boundary. `make -j1 -q` confirms targets are up to date, then
    `make -j2` runs two recipes locked in a two-way FIFO handshake using GNU
    make 4.4's FIFO jobserver (`--jobserver-auth=fifo:`, empty stderr), so the
    build finishes only if both run concurrently. Afterwards `mem` confirms the
    free-frame minimum since initialization stayed at or above 64 MiB through
    the build (and below the post-build free count), and the synced ext4 image
    passes `e2fsck -fn` with the build outputs present.
    """
    _require_dev_rootfs()
    disk = copy_dev_rootfs(tmp_path)
    script = tmp_path / "d2.sh"
    script.write_text(
        r"""set -eu
export PATH=/bin:/usr/bin
mkdir /d2
cd /d2

cat > helper.h <<'EOF'
int helper_add(int a, int b);
EOF

cat > helper.c <<'EOF'
#include "helper.h"
int helper_add(int a, int b) { return a + b; }
EOF

cat > lib.hpp <<'EOF'
#pragma once
int risky(int x, int *dtor_hits);
EOF

cat > lib.cpp <<'EOF'
#include "lib.hpp"
#include <stdexcept>

namespace {
struct Guard {
    int *hits;
    ~Guard() { ++(*hits); }
};
}

int risky(int x, int *dtor_hits) {
    Guard g{dtor_hits};
    if (x > 1) {
        throw std::runtime_error("too big");
    }
    return x + 10;
}
EOF

cat > main.cpp <<'EOF'
#include "lib.hpp"
#include <exception>
#include <iostream>
#include <string>

extern "C" {
#include "helper.h"
}

int main() {
    int dtor_hits = 0;
    int sum = helper_add(20, 22);
    int ok = risky(1, &dtor_hits);
    std::string msg = "uncaught";
    try {
        (void)risky(2, &dtor_hits);
    } catch (const std::exception &e) {
        msg = e.what();
    }
    std::cout << "c-helper=" << sum << "\n";
    std::cout << "ok=" << ok << "\n";
    std::cout << "caught=" << msg << "\n";
    std::cout << "dtor=" << dtor_hits << "\n";
    return (sum == 42 && ok == 11 && msg == "too big" && dtor_hits == 2) ? 0 : 1;
}
EOF

cat > Makefile <<'EOF'
.PHONY: all
all: app

helper.o: helper.c helper.h
	gcc -O2 -Wall -Wextra -c -o $@ $<

libthrow.so: lib.cpp lib.hpp
	g++ -O2 -Wall -Wextra -fPIC -shared -o $@ $<

app: main.cpp helper.o libthrow.so lib.hpp helper.h
	g++ -O2 -Wall -Wextra -o $@ main.cpp helper.o -L. -lthrow -Wl,-rpath,.
EOF

if ! make -j1 > build.log 2>&1; then
    cat build.log
    exit 11
fi
./app
make -j1 -q
printf 'uptodate=ok\n'
""",
        encoding="utf-8",
    )
    j2_script = tmp_path / "d2-j2.sh"
    j2_script.write_text(
        r"""set -eu
export PATH=/bin:/usr/bin
cd /d2
mkfifo j2.a2b j2.b2a

# Two-way FIFO handshake between recipes a and b: neither can finish unless
# GNU make 4.4 acquires its jobserver token and runs both concurrently.
cat > Makefile.j2 <<'EOF'
.PHONY: all a b
all: a b
a:
	+printf '%s\n' "$$MAKEFLAGS" > j2.flags && printf 'from-a\n' > j2.a2b && read -r got < j2.b2a && [ "$$got" = from-b ]
b:
	read -r got < j2.a2b && [ "$$got" = from-a ] && printf 'from-b\n' > j2.b2a
EOF

if ! make -j2 -f Makefile.j2 > j2.out 2> j2.err; then
    cat j2.out j2.err
    exit 12
fi
rm j2.a2b j2.b2a
if [ -s j2.err ]; then
    printf 'j2-stderr-unexpected:\n'
    cat j2.err
    exit 13
fi
case "$(cat j2.flags)" in
    *--jobserver-auth=fifo:*) ;;
    *)
        printf 'j2-no-fifo-jobserver:%s\n' "$(cat j2.flags)"
        exit 14
        ;;
esac
printf 'j2=ok\n'
""",
        encoding="utf-8",
    )
    debugfs(disk, f'write "{script}" /d2.sh', write=True)
    debugfs(disk, f'write "{j2_script}" /d2-j2.sh', write=True)
    m = boot("qemutest", memory="512M", devices=Devices(blk_image=disk, rng=True))
    m.expect(PROMPT, BOOT_TIMEOUT)

    # Generous outer timeout for TCG on a busy host (no timing ceilings).
    raw = run(m, "linux /bin/busybox ash /d2.sh", 600)
    lines = raw.splitlines()
    done = BUSYBOX_EXIT.fullmatch(lines[-1]) if lines else None
    assert done, f"no `linux: /bin/busybox exit=...` line last:\n{raw}"
    assert int(done.group(1)) == 0, raw
    assert lines[:-1] == [
        "c-helper=42",
        "ok=11",
        "caught=too big",
        "dtor=2",
        "uptodate=ok",
    ], raw
    j2_done = run_busybox(m, "ash /d2-j2.sh")
    assert j2_done.status == 0, j2_done.raw
    assert j2_done.output.splitlines() == ["j2=ok"], j2_done.raw
    assert CRASH_BEGIN not in m.output

    mem_out = run(m, "mem")
    low_match = re.search(
        r"^  frames  [\d.]+ (?:[KMG]i)?B of [\d.]+ (?:[KMG]i)?B used "
        r"\((\d+) frames of (\d+) free, 4 KiB each; low [\d.]+ (?:[KMG]i)?B, (\d+) frames\)$",
        mem_out,
        re.MULTILINE,
    )
    assert low_match, mem_out
    free_frames, owned_frames, min_free_frames = (int(g) for g in low_match.groups())
    assert min_free_frames < free_frames <= owned_frames, mem_out
    assert min_free_frames * 4096 >= 64 * 1024 * 1024, (
        f"free-frame low-water mark fell below 64 MiB: {min_free_frames} frames "
        f"({min_free_frames * 4096 / (1024 * 1024):.1f} MiB)\n{mem_out}"
    )

    assert run(m, "sync", 60) == "sync: ok\n"
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    e2fsck_check(disk)

    assert {
        "helper.o",
        "libthrow.so",
        "app",
        "build.log",
        "j2.out",
        "j2.err",
        "j2.flags",
    } <= debugfs_names(disk, "/d2")
    build_log = debugfs(disk, "cat /d2/build.log")
    assert "gcc -O2 -Wall -Wextra -c -o helper.o helper.c" in build_log, build_log
    assert "g++ -O2 -Wall -Wextra -fPIC -shared -o libthrow.so lib.cpp" in build_log, build_log
    assert "g++ -O2 -Wall -Wextra -o app main.cpp helper.o -L. -lthrow -Wl,-rpath,." in build_log, (
        build_log
    )
    assert debugfs(disk, "cat /d2/j2.err") == ""
    assert "--jobserver-auth=fifo:" in debugfs(disk, "cat /d2/j2.flags")


def test_local_git_and_sqlite3(boot, tmp_path):
    """D3: unmodified Alpine `git` and `sqlite3` on the developer image at 512 MiB.

    `git` initializes a repository, commits on `main` and a diverged `feature`
    branch, checks `git diff` and `git checkout`, performs a three-way merge,
    checks `git log` and `git fsck --full --strict`. `sqlite3` enables WAL mode,
    verifies commit and rollback, then holds an active `BEGIN IMMEDIATE` write
    transaction in one process (synchronized through FIFOs) while a concurrent
    reader sees the pre-commit WAL snapshot and a competing writer fails with
    `database is locked` via POSIX `fcntl` locks. After committing while keeping
    the writer connected, a second process reads the committed frames from the
    non-empty `-wal` via `-shm`, then the connected writer runs
    `PRAGMA wal_checkpoint(TRUNCATE)` (`0|0|0`) and `-wal` is confirmed zero
    bytes before closing. Finally `PRAGMA integrity_check` returns `ok`, the
    synced ext4 image passes `e2fsck -fn`, and the host's `sqlite3` verifies the
    persisted database extracted with `debugfs`.
    """
    _require_dev_rootfs()
    disk = copy_dev_rootfs(tmp_path)
    script = tmp_path / "d3-git-sqlite.sh"
    script.write_text(
        r"""set -eu
export PATH=/bin:/usr/bin
export HOME=/root

# --- Part 1: local git (init, commit, diff, branch, 3-way merge, log, fsck) ---
mkdir /d3-git
cd /d3-git
git init -q -b main
git config user.name "Lumen Test"
git config user.email "test@lumen.local"

printf 'alpha\n' > a.txt
printf 'line1\nline2\nline3\n' > shared.txt
git add a.txt shared.txt
git commit -q -m "initial commit"
[ -z "$(git status --short)" ]

git checkout -q -b feature
printf 'line1\nline2\nline3-feature\n' > shared.txt
printf 'beta\n' > b.txt
git diff -- shared.txt > /tmp/git-diff.txt
grep -qx -- '-line3' /tmp/git-diff.txt
grep -qx -- '+line3-feature' /tmp/git-diff.txt
git add shared.txt b.txt
git commit -q -m "feature work"

git checkout -q main
[ ! -e b.txt ]
grep -qx 'line3' shared.txt
printf 'line1-main\nline2\nline3\n' > shared.txt
printf 'gamma\n' > c.txt
git add shared.txt c.txt
git commit -q -m "main work"

git merge -q --no-edit feature >/dev/null
[ -z "$(git status --short)" ]
git diff HEAD~1 -- b.txt > /tmp/git-merge-diff.txt
grep -qx -- '+beta' /tmp/git-merge-diff.txt
git fsck --full --strict --no-progress >/dev/null

printf 'git-count=%s\n' "$(git rev-list --count HEAD)"
git --no-pager log --format='git-log=%s'
printf 'git-shared-begin\n'
cat shared.txt
printf 'git-shared-end\n'

# --- Part 2: sqlite3 (WAL mode, commit/rollback, cross-process POSIX locks) ---
mkdir /d3-sql
cd /d3-sql
init_out=$(sqlite3 /d3-sql/app.db <<'SQL'
PRAGMA journal_mode=WAL;
CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, qty INTEGER NOT NULL);
BEGIN;
INSERT INTO items VALUES (1, 'alpha', 10);
INSERT INTO items VALUES (2, 'beta', 20);
COMMIT;
BEGIN;
INSERT INTO items VALUES (99, 'rolled-back', 999);
ROLLBACK;
SELECT COUNT(*), SUM(qty) FROM items;
SQL
)
printf 'sql-init=%s\n' "$(printf '%s\n' "$init_out" | paste -sd, -)"

# Hold an open WAL write transaction in a background sqlite3 process; FIFOs
# establish that the write lock is held before the concurrent reader and
# competing writer run, and that committed WAL frames are visible across
# processes before close-time checkpointing (no timing assumptions).
mkfifo sql-ready sql-go
cat > /d3-sql/hold.sh <<'HOLD'
#!/bin/sh
printf '%s\n' "$1" > /d3-sql/sql-ready
read -r tok < /d3-sql/sql-go
[ "$tok" = "$2" ]
HOLD
chmod 755 /d3-sql/hold.sh

sqlite3 /d3-sql/app.db > /tmp/sql-writer.out <<'SQL' &
BEGIN IMMEDIATE;
INSERT INTO items VALUES (10, 'from-writer', 100);
.shell /d3-sql/hold.sh locked commit
COMMIT;
.shell /d3-sql/hold.sh committed checkpoint
PRAGMA wal_checkpoint(TRUNCATE);
.shell /d3-sql/hold.sh checkpointed close
SQL
writer=$!

read -r ready_tok < /d3-sql/sql-ready
[ "$ready_tok" = locked ]
[ -f /d3-sql/app.db-shm ]

# WAL allows a concurrent reader to see the pre-commit snapshot while the
# writer holds the write lock.
snap=$(sqlite3 /d3-sql/app.db "SELECT COUNT(*), SUM(qty) FROM items;")
printf 'sql-wal-snapshot=%s\n' "$snap"

# A competing writer must fail immediately due to POSIX fcntl record locking.
if sqlite3 /d3-sql/app.db "PRAGMA busy_timeout=0; BEGIN IMMEDIATE; INSERT INTO items VALUES (11, 'blocked', 1); COMMIT;" > /tmp/sql-conflict.out 2> /tmp/sql-conflict.err; then
    printf 'sql-conflict=UNEXPECTED_SUCCESS\n'
    printf 'commit\n' > /d3-sql/sql-go
    read -r _ < /d3-sql/sql-ready || true
    printf 'checkpoint\n' > /d3-sql/sql-go
    read -r _ < /d3-sql/sql-ready || true
    printf 'close\n' > /d3-sql/sql-go
    wait "$writer" || true
    exit 41
fi
grep -q 'database is locked' /tmp/sql-conflict.err
printf 'sql-lock-conflict=locked\n'

# Commit while keeping the writer's connection open so committed frames sit in
# -wal and a second process reads them through the shared -shm index.
printf 'commit\n' > /d3-sql/sql-go
read -r ready_tok < /d3-sql/sql-ready
[ "$ready_tok" = committed ]
[ -s /d3-sql/app.db-wal ]
wal_live=$(sqlite3 /d3-sql/app.db "SELECT COUNT(*), SUM(qty) FROM items;")
printf 'sql-wal-committed=%s\n' "$wal_live"
[ -s /d3-sql/app.db-wal ]

# Run PRAGMA wal_checkpoint(TRUNCATE) while the writer stays connected so it
# checkpoints real WAL frames and truncates -wal to zero bytes before close.
printf 'checkpoint\n' > /d3-sql/sql-go
read -r ready_tok < /d3-sql/sql-ready
[ "$ready_tok" = checkpointed ]
[ -f /d3-sql/app.db-wal ]
[ ! -s /d3-sql/app.db-wal ]

printf 'close\n' > /d3-sql/sql-go
wait "$writer"
rm /d3-sql/sql-ready /d3-sql/sql-go /d3-sql/hold.sh

ckpt=$(cat /tmp/sql-writer.out)
case "$ckpt" in
    0\|0\|0) ;;
    *)
        printf 'sql-checkpoint-failed=%s\n' "$ckpt"
        exit 42
        ;;
esac
printf 'sql-integrity=%s\n' "$(sqlite3 /d3-sql/app.db 'PRAGMA integrity_check;')"
sqlite3 /d3-sql/app.db "SELECT id, name, qty FROM items ORDER BY id;"
""",
        encoding="utf-8",
    )
    debugfs(disk, f'write "{script}" /d3-git-sqlite.sh', write=True)
    m = boot("qemutest", memory="512M", devices=Devices(blk_image=disk, rng=True))
    m.expect(PROMPT, BOOT_TIMEOUT)

    # Generous outer timeout for TCG on a busy host (no timing ceilings).
    raw = run(m, "linux /bin/busybox ash /d3-git-sqlite.sh", 600)
    lines = raw.splitlines()
    done = BUSYBOX_EXIT.fullmatch(lines[-1]) if lines else None
    assert done, f"no `linux: /bin/busybox exit=...` line last:\n{raw}"
    assert int(done.group(1)) == 0, raw
    out = lines[:-1]
    assert out == [
        "git-count=4",
        "git-log=Merge branch 'feature'",
        "git-log=main work",
        "git-log=feature work",
        "git-log=initial commit",
        "git-shared-begin",
        "line1-main",
        "line2",
        "line3-feature",
        "git-shared-end",
        "sql-init=wal,2|30",
        "sql-wal-snapshot=2|30",
        "sql-lock-conflict=locked",
        "sql-wal-committed=3|130",
        "sql-integrity=ok",
        "1|alpha|10",
        "2|beta|20",
        "10|from-writer|100",
    ], raw
    assert CRASH_BEGIN not in m.output

    assert run(m, "sync", 60) == "sync: ok\n"
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    e2fsck_check(disk)

    assert {".git", "a.txt", "b.txt", "c.txt", "shared.txt"} <= debugfs_names(disk, "/d3-git")
    assert debugfs(disk, "cat /d3-git/a.txt") == "alpha\n"
    assert debugfs(disk, "cat /d3-git/b.txt") == "beta\n"
    assert debugfs(disk, "cat /d3-git/c.txt") == "gamma\n"
    assert debugfs(disk, "cat /d3-git/shared.txt") == "line1-main\nline2\nline3-feature\n"
    assert debugfs(disk, "cat /d3-git/.git/HEAD") == "ref: refs/heads/main\n"

    dumped_db = tmp_path / "app.db"
    debugfs(disk, f'dump /d3-sql/app.db "{dumped_db}"')
    with sqlite3.connect(dumped_db) as conn:
        assert conn.execute("PRAGMA integrity_check;").fetchall() == [("ok",)]
        assert conn.execute("SELECT id, name, qty FROM items ORDER BY id;").fetchall() == [
            (1, "alpha", 10),
            (2, "beta", 20),
            (10, "from-writer", 100),
        ]


def test_vim_edits_a_file_over_pty(boot, tmp_path):
    """D3: unmodified Alpine `vim` editing a file over a PTY on the developer image at 512 MiB.

    `/usr/local/bin/ptyrun --script` allocates a PTY via `/dev/ptmx`, starts
    `vim -u NONE -N -c 'set ttimeoutlen=0 showmode laststatus=2 statusline=size=%{&lines}x%{&columns}'`
    on `/dev/pts/N` as a session leader with a controlling terminal, and drives
    it through the PTY master directly (avoiding `/dev/console` before C10).
    Every step waits in the PTY output stream for vim's rendered state (the
    initial file lines and `size=24x80`, the `SIGWINCH` resize redraw
    `size=30x90` after `TIOCSWINSZ`, `-- INSERT --`, the typed text, `written`)
    before sending the next keys, then waits for exit 0. Afterwards the edited
    file is verified in the guest and, after `sync` and `e2fsck -fn`, on the
    ext4 image via `debugfs`.
    """
    _require_dev_rootfs()
    disk = copy_dev_rootfs(tmp_path)
    pty_script = tmp_path / "d3-vim.pty"
    pty_script.write_text(
        r"""# Confirm vim rendered the initial file and initial 24x80 statusline before typing.
expect first line
expect second line
expect size=24x80
# Resize via TIOCSWINSZ and confirm vim handles SIGWINCH by redrawing 30x90.
winsize 30 90
expect size=30x90
# Move to the last line, open a new line below, wait for Insert mode.
send Go
expect -- INSERT --
# Type the new line and confirm it renders.
send third line from vim
expect third line from vim
# Leave Insert mode, write the file, confirm vim reports written, then quit.
send \x1b:w\r
expect written
send :q\r
wait_exit 0
""",
        encoding="utf-8",
    )
    runner = tmp_path / "d3-vim.sh"
    runner.write_text(
        r"""set -eu
export PATH=/bin:/usr/bin
export HOME=/root
export TERM=vt100

mkdir /d3-vim
printf 'first line\nsecond line\n' > /d3-vim/note.txt

/usr/local/bin/ptyrun --script /d3-vim.pty -- /usr/bin/vim -u NONE -N -c 'set ttimeoutlen=0 showmode laststatus=2 statusline=size=%{&lines}x%{&columns}' /d3-vim/note.txt

[ ! -e /d3-vim/.note.txt.swp ]
printf 'vim-file-begin\n'
cat /d3-vim/note.txt
printf 'vim-file-end\n'

# Regression check: expect must match START before a >200 KiB tail evicts it,
# and still find END across buffer evictions.
cat > /tmp/large.pty <<'PTY'
expect START
expect END
wait_exit 0
PTY
/usr/local/bin/ptyrun --script /tmp/large.pty -- /bin/sh -c 'printf START; dd if=/dev/zero bs=4096 count=50 2>/dev/null | tr "\0" x; printf END'

# Regression check: relay mode forwards piped stdin EOF via VEOF so canonical
# children finish instead of hanging.
relay_out=$(printf 'hello-relay\n' | /usr/local/bin/ptyrun /bin/cat)
case "$relay_out" in
    *hello-relay*) printf 'pty-relay-eof=ok\n' ;;
    *)
        printf 'pty-relay-eof=BAD:%s\n' "$relay_out"
        exit 51
        ;;
esac
""",
        encoding="utf-8",
    )
    debugfs(disk, f'write "{pty_script}" /d3-vim.pty', write=True)
    debugfs(disk, f'write "{runner}" /d3-vim.sh', write=True)
    m = boot("qemutest", memory="512M", devices=Devices(blk_image=disk, rng=True))
    m.expect(PROMPT, BOOT_TIMEOUT)

    # Generous outer timeout for TCG on a busy host (no timing ceilings).
    raw = run(m, "linux /bin/busybox ash /d3-vim.sh", 120)
    lines = raw.splitlines()
    done = BUSYBOX_EXIT.fullmatch(lines[-1]) if lines else None
    assert done, f"no `linux: /bin/busybox exit=...` line last:\n{raw}"
    assert int(done.group(1)) == 0, raw
    assert lines[:-1] == [
        "ok step 1 expect first line",
        "ok step 2 expect second line",
        "ok step 3 expect size=24x80",
        "ok step 4 winsize 30x90",
        "ok step 5 expect size=30x90",
        "ok step 6 send",
        "ok step 7 expect -- INSERT --",
        "ok step 8 send",
        "ok step 9 expect third line from vim",
        "ok step 10 send",
        "ok step 11 expect written",
        "ok step 12 send",
        "ok step 13 wait_exit 0",
        "vim-file-begin",
        "first line",
        "second line",
        "third line from vim",
        "vim-file-end",
        "ok step 1 expect START",
        "ok step 2 expect END",
        "ok step 3 wait_exit 0",
        "pty-relay-eof=ok",
    ], raw
    assert CRASH_BEGIN not in m.output

    assert run(m, "sync", 60) == "sync: ok\n"
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    e2fsck_check(disk)

    assert debugfs_names(disk, "/d3-vim") == {"note.txt"}
    assert debugfs(disk, "cat /d3-vim/note.txt") == "first line\nsecond line\nthird line from vim\n"
