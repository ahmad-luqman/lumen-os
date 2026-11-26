"""Self-tests for the Alpine developer image (#344): tools/alpine-lock.py's
resolver, lockfile and .apk extraction, tools/fetch-alpine-packages.sh,
tools/mirror-alpine-packages.sh (against a stub `gh`), tools/mkrootfs-dev.sh
and `rootfs.copy_dev_rootfs`.

Nearly everything runs on fixtures (made-up APKINDEX stanzas, .apk files
built here, `file://` mirrors, stub `curl` and `gh`), with no network. Two
parts read what `make deps-dev` and `make rootfs-dev` leave in build/: the
check that no locked package carries an unhandled install script, and the
last group, on the built image, `build/alpine-dev.ext4`. Each skips without
them locally and fails under CI, which builds both.
"""

from __future__ import annotations

import contextlib
import gzip
import hashlib
import http.server
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import threading
from pathlib import Path

import pytest
from lumen_harness import BUILD_DIR, REPO_ROOT, rootfs
from test_rootfs import _e2fsprogs as _rootfs_e2fsprogs


def _load_alpine_lock():
    spec = importlib.util.spec_from_file_location(
        "alpine_lock", REPO_ROOT / "tools" / "alpine-lock.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["alpine_lock"] = module  # dataclasses look their module up
    spec.loader.exec_module(module)
    return module


al = _load_alpine_lock()

LOCK = REPO_ROOT / "tools" / "alpine-packages.lock"
WANTED = REPO_ROOT / "tools" / "alpine-packages.txt"
DEV_IMAGE = BUILD_DIR / "alpine-dev.ext4"
GATE_TOOLS = ("gcc", "g++", "make", "git", "sqlite3", "vim")


@pytest.fixture(autouse=True)
def _no_durable_mirror(monkeypatch):
    """A backstop: unset, LUMEN_APK_FALLBACK means the real durable mirror on
    github.com, which no test here may reach, so every run sees it empty
    unless the test sets it (`_run_fetch` sets it itself, and its curl stub
    keeps even an unset one offline)."""
    monkeypatch.setenv("LUMEN_APK_FALLBACK", "")


# --- Fixtures: .apk files ------------------------------------------------------


def _tar(entries, *, eof: bool) -> bytes:
    """A tar of `entries`: (name, kind, payload, mode) with kind `f` (payload:
    bytes), `d`, `l` (symlink; payload: target), `h` (hard link; payload:
    the linked name) or `c` (a character device). Without `eof`, the
    end-of-archive blocks are cut off, as apk writes its first segments."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for name, kind, payload, mode in entries:
            info = tarfile.TarInfo(name)
            info.mode = mode
            info.mtime = 0
            if kind == "f":
                info.size = len(payload)
                tar.addfile(info, io.BytesIO(payload))
                continue
            info.type = {
                "d": tarfile.DIRTYPE,
                "l": tarfile.SYMTYPE,
                "h": tarfile.LNKTYPE,
                "c": tarfile.CHRTYPE,
            }[kind]
            if kind in "lh":
                info.linkname = payload
            tar.addfile(info)
        end = tar.offset
    raw = buf.getvalue()
    return raw if eof else raw[:end]


def _apk(data_entries, *, scripts=None, signed=True, data_eof=True, datahash=True) -> bytes:
    """An .apk: a signature segment (when `signed`), a control segment
    (.PKGINFO, with the data segment's sha256 as `datahash` unless told
    otherwise, and `scripts`, name to body) and a data segment, each its
    own gzip member. `datahash` may be False (none) or a hash to claim."""
    data = gzip.compress(_tar(data_entries, eof=data_eof), mtime=0)
    pkginfo = "pkgname = fixture\n"
    if datahash:
        claimed = hashlib.sha256(data).hexdigest() if datahash is True else datahash
        pkginfo += f"datahash = {claimed}\n"
    control = [(".PKGINFO", "f", pkginfo.encode(), 0o644)]
    control += [(name, "f", body, 0o755) for name, body in (scripts or {}).items()]
    members = []
    if signed:
        members.append(_tar([(".SIGN.RSA.fixture.rsa.pub", "f", b"sig", 0o644)], eof=False))
    members.append(_tar(control, eof=False))
    return b"".join(gzip.compress(m, mtime=0) for m in members) + data


HELLO = b"\x7fELF hello"
DATA = [
    ("usr", "d", None, 0o755),
    ("usr/bin", "d", None, 0o755),
    ("usr/bin/hello", "f", HELLO, 0o755),
    ("usr/bin/hello-again", "h", "usr/bin/hello", 0o755),
    ("usr/lib", "d", None, 0o755),
    ("usr/lib/libx.so.1.0", "f", b"lib", 0o644),
    ("usr/lib/libx.so.1", "l", "libx.so.1.0", 0o777),
    ("usr/share/locked", "d", None, 0o555),
    ("usr/share/locked/inside", "f", b"read-only parent", 0o444),
]


# --- Versions --------------------------------------------------------------------


@pytest.mark.parametrize(
    ("a", "b", "want"),
    [
        ("1.2.3-r0", "1.2.3-r0", 0),
        ("1.2.3-r0", "1.2.3-r1", -1),
        ("1.10-r0", "1.9-r0", 1),
        ("1.2-r0", "1.2.1-r0", -1),
        ("1.0_rc1-r0", "1.0-r0", -1),
        ("1.0_alpha-r0", "1.0_beta-r0", -1),
        ("1.0_beta-r0", "1.0_pre-r0", -1),
        ("1.0_pre-r0", "1.0_rc-r0", -1),
        ("1.0_p1-r0", "1.0-r0", 1),
        ("1.0a-r0", "1.0-r0", 1),
        ("6.5_p20241006-r3", "6.5-r3", 1),
        ("23.3.0", "23.2.0", 1),
        ("1.0-r0", "1.0_cvs-r0", -1),
        ("1.0_cvs-r0", "1.0_svn-r0", -1),
        ("1.0_svn-r0", "1.0_git-r0", -1),
        ("1.0_git-r0", "1.0_hg-r0", -1),
        ("1.0_hg-r0", "1.0_p-r0", -1),
        ("1.0_rc2-r0", "1.0_rc10-r0", -1),
        ("1.0_rc1_p1-r0", "1.0_rc1-r0", 1),
    ],
)
def test_vercmp_orders_as_apk_does(a, b, want):
    assert al.vercmp(a, b) == want
    assert al.vercmp(b, a) == -want


def test_dep_constraints():
    assert al.parse_dep("gcc=14.2.0-r4").allows("14.2.0-r4")
    assert not al.parse_dep("gcc=14.2.0-r4").allows("14.2.0-r5")
    assert al.parse_dep("libexpat>=2.9.0").allows("2.10.0-r0")
    assert not al.parse_dep("libexpat<2.9.0").allows("2.10.0-r0")
    assert al.parse_dep("x<=2.0-r0").allows("2.0-r0")
    assert not al.parse_dep("x<=2.0-r0").allows("2.0-r1")
    assert al.parse_dep("x>2.0-r0").allows("2.0-r1")
    assert not al.parse_dep("x>2.0-r0").allows("2.0-r0")
    assert al.parse_dep("foo=~1.2").allows("1.2_rc1-r0")
    assert al.parse_dep("foo~1.2").allows("1.2.7-r0")
    assert not al.parse_dep("foo~1.2").allows("1.20-r0")
    assert not al.parse_dep("so:libx.so.1=2").allows(None)
    assert al.parse_dep("so:libx.so.1").allows(None)
    assert al.parse_dep("!oldapp").conflict
    with pytest.raises(al.LockError, match="can't parse"):
        al.parse_dep(">=1.0")


# --- The resolver ------------------------------------------------------------------

INDEX = """\
P:app
V:1.0-r0
D:libfoo so:libbar.so.1 cmd:tool so:libc.musl-x86_64.so.1 !oldapp

P:libfoo
V:2.0-r1
D:libbaz>=1.0

P:libbaz
V:1.1-r0
D:libfoo=2.0-r1

P:barlib
V:3.0-r0
p:so:libbar.so.1=3.0.0

P:toolbox
V:1.0-r0
p:cmd:tool=1.0-r0 cmd:other=1.0-r0

P:app-extra
V:1.0-r0
i:app=1.0-r0 libbaz
D:extradep

P:extradep
V:1.0-r0

P:app-doc
V:1.0-r0
i:docs app

P:oldapp
V:0.9-r0

P:unrelated
V:1.0-r0
F:usr/bin
R:ignored
"""

INSTALLED = """\
C:Q1abc
P:musl
V:1.2.5-r12
p:so:libc.musl-x86_64.so.1=1
F:lib
R:ld-musl-x86_64.so.1
"""


def _resolve(wanted, index=INDEX, installed=INSTALLED):
    return al.resolve(wanted, al.parse_index(index, "main"), al.parse_index(installed, "installed"))


def test_resolver_closure_through_deps_provides_so_cmd_install_if_and_a_cycle():
    """The fixture's `app` needs a package by name, an `so:` and a `cmd:`
    provide and libc (the minirootfs'); libfoo and libbaz need each other;
    app-extra joins by install_if and brings its own dependency; app-doc's
    install_if (`docs`) isn't met; oldapp is only a conflict."""
    got = _resolve(["app"])
    assert [p.name for p in got.packages] == [
        "app",
        "app-extra",
        "barlib",
        "extradep",
        "libbaz",
        "libfoo",
        "toolbox",
    ]
    assert got.repos == ["main"]


def test_resolver_skips_wanted_packages_the_minirootfs_has():
    assert [p.name for p in _resolve(["musl", "extradep"]).packages] == ["extradep"]


def test_resolver_picks_providers_by_priority_then_provided_version():
    index = """\
P:gcc
V:14-r0
D:so:libisl.so.23 cmd:sh

P:isl25
V:0.25-r2
p:so:libisl.so.23=23.2.0

P:isl26
V:0.26-r1
p:so:libisl.so.23=23.3.0

P:bash
V:5.2-r0
p:cmd:sh=5.2-r0

P:yash
V:2.57-r0
k:10
p:cmd:sh=2.57-r0
"""
    got = [p.name for p in _resolve(["gcc"], index).packages]
    assert got == ["gcc", "isl26", "yash"]


def test_resolver_refuses_a_tie_between_providers():
    index = "P:a\nV:1-r0\nD:so:x.so.1\n\nP:p1\nV:1-r0\np:so:x.so.1\n\nP:p2\nV:1-r0\np:so:x.so.1\n"
    with pytest.raises(al.LockError, match="p1, p2 all provide: name the one"):
        _resolve(["a"], index)
    # Naming one in alpine-packages.txt settles it.
    assert [p.name for p in _resolve(["p2", "a"], index).packages] == ["a", "p2"]


def test_resolver_refusals():
    with pytest.raises(al.LockError, match="which no package provides"):
        _resolve(["a"], "P:a\nV:1-r0\nD:so:missing.so.1\n")
    with pytest.raises(al.LockError, match="the index lacks"):
        _resolve(["nothere"])
    with pytest.raises(al.LockError, match="the minirootfs has musl-1.2.5-r12"):
        _resolve(["a"], "P:a\nV:1-r0\nD:musl>=1.2.6\n")
    with pytest.raises(al.LockError, match="but the index has b-1-r0"):
        _resolve(["a"], "P:a\nV:1-r0\nD:b=2-r0\n\nP:b\nV:1-r0\n")
    with pytest.raises(al.LockError, match="app conflicts with oldapp"):
        _resolve(["app", "oldapp"])


def test_resolver_install_if_chains_and_checks_versions():
    """install_if runs to a fixpoint (b joins for a, then c for b) and
    honours a version in its atoms (d wants a=2, which a-1 isn't)."""
    index = "P:a\nV:1-r0\n\nP:b\nV:1-r0\ni:a\n\nP:c\nV:1-r0\ni:b\n\nP:d\nV:1-r0\ni:a=2-r0\n"
    assert [p.name for p in _resolve(["a"], index).packages] == ["a", "b", "c"]


def test_resolver_reads_a_conflict_in_install_if_as_absent():
    """`i:a !c`: e joins with a alone, not once c is in (apk's reading of
    libcanberra-alsa's `i:libcanberra alsa-lib !pulseaudio`)."""
    index = "P:a\nV:1-r0\n\nP:c\nV:1-r0\n\nP:e\nV:1-r0\ni:a !c\n"
    assert [p.name for p in _resolve(["a"], index).packages] == ["a", "e"]
    assert [p.name for p in _resolve(["a", "c"], index).packages] == ["a", "c"]


@pytest.mark.parametrize(
    "index",
    [
        # c joins in the same pass as b.
        "P:a\nV:1-r0\n\nP:b\nV:1-r0\ni:a !c\n\nP:c\nV:1-r0\ni:a\n",
        # b's own dependency brings c.
        "P:a\nV:1-r0\n\nP:b\nV:1-r0\ni:a !c\nD:d\n\nP:d\nV:1-r0\nD:c\n\nP:c\nV:1-r0\n",
    ],
)
def test_resolver_refuses_an_install_if_conflict_met_later(index):
    """apk leaves b out in both; this resolver refuses rather than guess."""
    with pytest.raises(al.LockError, match=r"b joined by install_if, but !c no longer holds"):
        _resolve(["a"], index)


def test_resolver_refuses_a_conflict_a_minirootfs_package_declares():
    with pytest.raises(al.LockError, match=r"x conflicts with y \(y\)"):
        _resolve(["y"], "P:y\nV:1-r0\n", INSTALLED + "\nP:x\nV:1-r0\nD:!y\n")


def test_resolver_installed_package_never_joins_by_install_if():
    assert [
        p.name for p in _resolve(["a"], "P:a\nV:1-r0\n\nP:musl\nV:1.2.5-r12\ni:a\n").packages
    ] == ["a"]


def test_resolver_minirootfs_package_is_never_upgraded_for_a_provide():
    index = "P:a\nV:1-r0\nD:so:libc.musl-x86_64.so.1>=2\n\nP:musl\nV:1.2.6-r0\np:so:libc.musl-x86_64.so.1=2\n"
    with pytest.raises(al.LockError, match="which musl-1.2.6-r0 doesn't meet"):
        _resolve(["a"], index)


def test_resolver_a_package_may_conflict_with_a_name_it_provides():
    assert [p.name for p in _resolve(["new"], "P:new\nV:1-r0\np:old\nD:!old\n").packages] == ["new"]


def test_resolver_names_the_providers_of_a_wanted_name_the_index_lacks():
    index = "P:clang19\nV:19-r0\np:clang=19\n"
    with pytest.raises(al.LockError, match=r"names clang, which the index lacks \(clang19 provide"):
        _resolve(["clang"], index)


def test_resolver_honours_provided_versions_and_prefers_a_versioned_provide():
    # A versioned provide below the constraint doesn't meet it.
    with pytest.raises(al.LockError, match="which no package provides"):
        _resolve(["a"], "P:a\nV:1-r0\nD:so:q.so.1>=2\n\nP:q\nV:1-r0\np:so:q.so.1=1.0\n")
    # At the same priority, a provide with a version beats one without.
    index = "P:a\nV:1-r0\nD:cmd:t\n\nP:bare\nV:1-r0\np:cmd:t\n\nP:ver\nV:1-r0\np:cmd:t=1\n"
    assert [p.name for p in _resolve(["a"], index).packages] == ["a", "ver"]


def test_resolver_refuses_a_conflict_with_the_minirootfs():
    with pytest.raises(al.LockError, match="a conflicts with musl \\(musl\\)"):
        _resolve(["a"], "P:a\nV:1-r0\nD:!musl\n")


def test_parse_index_keeps_the_higher_of_two_versions_and_refuses_bad_names():
    index = al.parse_index("P:x\nV:8.0-r0\n\nP:x\nV:9.0-r0\n\nP:x\nV:8.5-r0\n", "community")
    assert index["x"].version == "9.0-r0"
    with pytest.raises(al.LockError, match="bad package name"):
        al.parse_index("P:x y\nV:1-r0\n", "main")
    with pytest.raises(al.LockError, match="bad package name or version"):
        al.parse_index("P:x\nV:1.0\n", "main")


def test_merge_repos_lets_main_hide_community():
    main = al.parse_index("P:x\nV:1-r0\n", "main")
    community = al.parse_index("P:x\nV:2-r0\n\nP:y\nV:1-r0\n", "community")
    merged = al.merge_repos([main, community])
    assert (merged["x"].repo, merged["x"].version, merged["y"].repo) == (
        "main",
        "1-r0",
        "community",
    )


def test_parse_wanted():
    assert al.parse_wanted("# c\ngcc   # D2\n\nmake\n") == ["gcc", "make"]
    with pytest.raises(al.LockError, match="listed twice"):
        al.parse_wanted("gcc\ngcc\n")
    with pytest.raises(al.LockError, match="names no package"):
        al.parse_wanted("# only a comment\n")


# --- The lockfile ------------------------------------------------------------------


def test_lockfile_parses_is_sorted_and_names_each_package_once():
    lock = al.parse_lock(LOCK.read_text())
    names = [p.name for p in lock.packages]
    assert names == sorted(names)
    assert len(names) == len(set(names))
    # Every top-level package is locked (none is in the minirootfs).
    assert set(al.parse_wanted(WANTED.read_text())) <= set(names)
    # Resolved against the minirootfs tools/fetch-deps.sh pins.
    pinned = re.search(
        r"^ALPINE_MINIROOTFS_VERSION=(\S+)$",
        (REPO_ROOT / "tools/fetch-deps.sh").read_text(),
        re.MULTILINE,
    )
    assert pinned and lock.minirootfs == pinned[1]
    assert {p.repo for p in lock.packages} <= {"main", "community"}


def test_parse_lock_refuses_a_malformed_minirootfs_line():
    with pytest.raises(al.LockError, match="malformed minirootfs"):
        al.parse_lock("# minirootfs 3.21.10 extra\na 1-r0 main " + "a" * 64 + "\n")


def test_parse_lock_refusals():
    sha = "a" * 64
    head = "# minirootfs 3.21.10\n"
    good = head + f"a 1-r0 main {sha}\nb 1-r0 main {sha}\n"
    assert [p.name for p in al.parse_lock(good).packages] == ["a", "b"]
    for text, why in [
        (head + f"b 1-r0 main {sha}\na 1-r0 main {sha}\n", "out of order"),
        (head + f"a 1-r0 main {sha}\na 1-r0 main {sha}\n", "out of order or listed twice"),
        (head + "a 1-r0 main abc\n", "bad sha256"),
        (head + f"a 1-r0 testing {sha}\n", "unknown repository"),
        (head + f"a 1-r0 main {sha} extra\n", "want `name version repo sha256`"),
        (f"a 1-r0 main {sha}\n", "no `# minirootfs"),
        (head + head + f"a 1-r0 main {sha}\n", "a second or malformed minirootfs"),
        (head, "no packages"),
    ]:
        with pytest.raises(al.LockError, match=re.escape(why)):
            al.parse_lock(text)


def test_format_lock_round_trips():
    resolution = _resolve(["app"])
    hashes = {p.name: hashlib.sha256(p.name.encode()).hexdigest() for p in resolution.packages}
    text = al.format_lock(resolution, hashes, "3.21.10", 15, {"main": ("v3.21.10-1", "f" * 64)})
    lock = al.parse_lock(text)
    assert lock.minirootfs == "3.21.10"
    assert [(p.name, p.version, p.sha256) for p in lock.packages] == [
        (p.name, p.version, hashes[p.name]) for p in resolution.packages
    ]
    assert "main/x86_64/APKINDEX.tar.gz v3.21.10-1 sha256 " + "f" * 64 in text


# --- .apk files ------------------------------------------------------------------------


def test_gzip_members_and_the_control_checksum():
    apk = _apk(DATA, scripts={".post-install": b"#!/bin/sh\n"})
    members = al.gzip_members(apk)
    assert len(members) == 3
    assert b"".join(members) == apk
    import base64

    want = "Q1" + base64.b64encode(hashlib.sha1(members[1]).digest()).decode()
    assert al.control_checksum(apk) == want
    with pytest.raises(al.LockError, match="truncated"):
        al.gzip_members(apk[:-10])
    with pytest.raises(al.LockError, match="no gzip member at byte"):
        al.gzip_members(apk + b"junk")


def test_extract_yields_only_the_data_files(tmp_path):
    """The fixture .apk's three gzip members: the signature and control
    segments (with a .post-install) stay out; the data segment's files,
    directories, symlink and hard link come out with their modes, a
    read-only directory still filled."""
    script = b"#!/bin/sh\ntouch /ran\n"
    apk = al.split_apk(_apk(DATA, scripts={".post-install": script}))
    assert apk.scripts == ((".post-install", hashlib.sha256(script).hexdigest()),)
    assert apk.has_datahash
    out = al.extract_data(apk, tmp_path)
    assert out.replaced == frozenset()
    al.apply_dir_modes(out.dir_modes)
    assert list(out.written) == [
        "usr/",
        "usr/bin/",
        "usr/bin/hello",
        "usr/bin/hello-again",
        "usr/lib/",
        "usr/lib/libx.so.1.0",
        "usr/lib/libx.so.1",
        "usr/share/locked/",
        "usr/share/locked/inside",
    ]
    found = sorted(str(p.relative_to(tmp_path)) for p in tmp_path.rglob("*"))
    assert not [n for n in found if n.startswith(".") or "/." in n]
    assert (tmp_path / "usr/bin/hello").read_bytes() == HELLO
    assert (tmp_path / "usr/bin/hello").stat().st_mode & 0o7777 == 0o755
    assert os.path.samefile(tmp_path / "usr/bin/hello", tmp_path / "usr/bin/hello-again")
    assert os.readlink(tmp_path / "usr/lib/libx.so.1") == "libx.so.1.0"
    assert (tmp_path / "usr/share/locked").stat().st_mode & 0o7777 == 0o555
    assert (tmp_path / "usr/share/locked/inside").read_bytes() == b"read-only parent"
    (tmp_path / "usr/share/locked").chmod(0o755)


def test_extract_takes_an_unsigned_apk_and_one_without_end_blocks(tmp_path):
    apk = al.split_apk(_apk([("a", "f", b"1", 0o644)], signed=False, data_eof=False))
    assert apk.scripts == ()
    assert al.extract_data(apk, tmp_path).written == ("a",)


def test_extract_never_overwrites_unless_replaceable(tmp_path):
    apk = al.split_apk(_apk([("usr/bin/strings", "f", b"binutils", 0o755)]))
    (tmp_path / "usr/bin").mkdir(parents=True)
    (tmp_path / "usr/bin/strings").symlink_to("/bin/busybox")
    with pytest.raises(al.LockError, match="usr/bin/strings is already in the image"):
        al.extract_data(apk, tmp_path)
    replaceable = frozenset({"usr/bin/strings", "usr/bin/other"})
    out = al.extract_data(apk, tmp_path, replaceable)
    assert out.replaced == {"usr/bin/strings"}
    assert (tmp_path / "usr/bin/strings").read_bytes() == b"binutils"
    # A second package with the same file is refused (stage passes only
    # what is still replaceable).
    with pytest.raises(al.LockError, match="already in the image"):
        al.extract_data(apk, tmp_path, replaceable - out.replaced)
    # The same path twice in one archive: replaced once, then refused.
    twice = al.split_apk(_apk([("usr/bin/x", "f", b"1", 0o755), ("usr/bin/x", "f", b"2", 0o755)]))
    (tmp_path / "usr/bin/x").symlink_to("/bin/busybox")
    with pytest.raises(al.LockError, match="usr/bin/x is already in the image"):
        al.extract_data(twice, tmp_path, frozenset({"usr/bin/x"}))


@pytest.mark.parametrize(
    ("entries", "why"),
    [
        ([("../evil", "f", b"x", 0o644)], "unsafe path"),
        ([("/abs", "f", b"x", 0o644)], "unsafe path"),
        ([(".PKGINFO", "f", b"x", 0o644)], "control file"),
        ([("dev/null", "c", None, 0o666)], "a device node or FIFO"),
        ([("a", "h", "missing", 0o644)], "hard link to missing"),
        ([("f", "f", b"x", 0o644), ("f/x", "f", b"y", 0o644)], "f is not a directory"),
    ],
)
def test_extract_refusals(tmp_path, entries, why):
    with pytest.raises(al.LockError, match=re.escape(why)):
        al.extract_data(al.split_apk(_apk(entries)), tmp_path)


def test_extract_refuses_a_hard_link_to_a_symlink(tmp_path):
    """A hard link must name an extracted regular file: linking to a
    symlink would make os.link follow it, maybe out of the stage."""
    (tmp_path / "secret").write_bytes(b"host file")
    root = tmp_path / "root"
    root.mkdir()
    apk = al.split_apk(
        _apk([("ptr", "l", str(tmp_path / "secret"), 0o777), ("grab", "h", "ptr", 0o644)])
    )
    with pytest.raises(al.LockError, match="grab: hard link to ptr, not an extracted file"):
        al.extract_data(apk, root)
    assert not (root / "grab").exists()


def test_extract_refuses_a_hard_link_through_a_symlinked_directory(tmp_path):
    """The link's target is checked all the way: `evil -> host dir`, then
    a hard link to `evil/secret`, must not link the host file in."""
    host = tmp_path / "host"
    host.mkdir()
    (host / "secret").write_bytes(b"SECRET")
    root = tmp_path / "root"
    root.mkdir()
    apk = al.split_apk(_apk([("evil", "l", str(host), 0o777), ("leak", "h", "evil/secret", 0o644)]))
    with pytest.raises(al.LockError, match="leak: hard link to evil/secret, not an extracted file"):
        al.extract_data(apk, root)
    assert not (root / "leak").exists()
    assert (host / "secret").stat().st_nlink == 1


def test_extract_keeps_setuid_setgid_and_sticky_bits(tmp_path):
    apk = al.split_apk(
        _apk(
            [
                ("bin", "d", None, 0o755),
                ("bin/su", "f", b"su", 0o4755),
                ("bin/wall", "f", b"w", 0o2755),
                ("shared", "d", None, 0o1777),
            ]
        )
    )
    al.apply_dir_modes(al.extract_data(apk, tmp_path).dir_modes)
    assert (tmp_path / "bin/su").stat().st_mode & 0o7777 == 0o4755
    assert (tmp_path / "bin/wall").stat().st_mode & 0o7777 == 0o2755
    assert (tmp_path / "shared").stat().st_mode & 0o7777 == 0o1777


@pytest.mark.parametrize("kind", ["file", "symlink"])
def test_extract_refuses_a_directory_over_a_file_or_symlink(tmp_path, kind):
    root = tmp_path / "root"
    root.mkdir()
    target = tmp_path / "outside"
    target.mkdir()
    target.chmod(0o755)
    if kind == "file":
        (root / "d").write_bytes(b"x")
    else:
        (root / "d").symlink_to(target)
    with pytest.raises(al.LockError, match="d: a directory over a file"):
        out = al.extract_data(al.split_apk(_apk([("d", "d", None, 0o700)])), root)
        al.apply_dir_modes(out.dir_modes)
    assert target.stat().st_mode & 0o777 == 0o755


def test_extract_refuses_to_write_through_a_symlink(tmp_path):
    outside = tmp_path / "outside"
    outside.mkdir()
    root = tmp_path / "root"
    (root / "usr").mkdir(parents=True)
    (root / "usr/lib").symlink_to(outside)
    with pytest.raises(al.LockError, match="usr/lib is not a directory"):
        al.extract_data(al.split_apk(_apk([("usr/lib/x", "f", b"x", 0o644)])), root)
    assert not list(outside.iterdir())


def test_split_apk_wants_two_or_three_members_in_their_roles():
    one = gzip.compress(_tar([("a", "f", b"1", 0o644)], eof=True))
    with pytest.raises(al.LockError, match="2 or 3 gzip members, this one 1"):
        al.split_apk(one)
    no_pkginfo = one + one
    with pytest.raises(al.LockError, match="no .PKGINFO"):
        al.split_apk(no_pkginfo)
    apk = _apk(DATA)
    bad_sig = (
        gzip.compress(_tar([("x", "f", b"1", 0o644)], eof=False))
        + apk[len(al.gzip_members(apk)[0]) :]
    )
    with pytest.raises(al.LockError, match="first segment isn't a signature"):
        al.split_apk(bad_sig)


def _swap_data(apk: bytes, other: bytes) -> bytes:
    """`apk` with its data segment replaced by `other`'s: the control
    segment, and so the index's C:, unchanged."""
    return b"".join(al.gzip_members(apk)[:-1]) + al.gzip_members(other)[-1]


def test_split_apk_checks_the_data_segment_against_datahash():
    """C: covers only the control segment; its .PKGINFO's datahash is what
    covers the data. A data segment swapped for another of the same size
    is refused."""
    real = _apk([("usr/bin/tool", "f", b"A" * 64, 0o755)])
    swapped = _swap_data(real, _apk([("usr/bin/tool", "f", b"B" * 64, 0o755)]))
    assert len(swapped) == len(real)
    assert al.control_checksum(swapped) == al.control_checksum(real)
    with pytest.raises(al.LockError, match="doesn't match its .PKGINFO datahash"):
        al.split_apk(swapped)
    assert not al.split_apk(_apk(DATA, datahash=False)).has_datahash


# --- busybox's applet links and staging --------------------------------------------

BUSYBOX_C = "Q1p/ydDCbqw8aB8uA/M5rzEgEeRD4="
BUSYBOX_TRIGGER = f"{BUSYBOX_C} /bin /usr/bin /sbin /usr/sbin /lib/modules/* /usr/lib/modules/*\n"
INSTALLED_DB = f"""\
P:busybox
V:1.37.0-r14
C:{BUSYBOX_C}
F:bin
R:busybox

P:busybox-binsh
V:1.37.0-r14
F:bin
R:sh
"""


def test_busybox_links_are_the_unowned_links_to_busybox(tmp_path):
    owned = al.owned_paths(INSTALLED_DB)
    assert owned == {"bin/busybox", "bin/sh"}
    (tmp_path / "bin").mkdir()
    (tmp_path / "usr/bin").mkdir(parents=True)
    (tmp_path / "bin/busybox").write_bytes(b"bb")
    (tmp_path / "bin/sh").symlink_to("/bin/busybox")
    (tmp_path / "bin/ls").symlink_to("/bin/busybox")
    (tmp_path / "usr/bin/strings").symlink_to("/bin/busybox")
    (tmp_path / "usr/bin/other").symlink_to("/bin/other")
    (tmp_path / "usr/bin/real").write_bytes(b"x")
    assert al.busybox_links(tmp_path, owned) == {"bin/ls", "usr/bin/strings"}


def _stub_minirootfs(mini: Path, version: str = "3.21.10") -> None:
    (mini / "lib/apk/db").mkdir(parents=True)
    (mini / "usr/bin").mkdir(parents=True)
    (mini / "bin").mkdir()
    (mini / "etc").mkdir()
    (mini / ".version").write_text(version + "\n")
    (mini / "lib/ld-musl-x86_64.so.1").write_bytes(b"\x7fELF ld")
    (mini / "lib/apk/db/installed").write_text(INSTALLED_DB)
    (mini / "lib/apk/db/triggers").write_text(BUSYBOX_TRIGGER)
    busybox = mini / "bin/busybox"
    busybox.write_bytes(b"#!/bin/sh\nexit 0\n")
    busybox.chmod(0o755)
    (mini / "bin/sh").symlink_to("/bin/busybox")
    (mini / "usr/bin/strings").symlink_to("/bin/busybox")


FIXTURE_APKS = {
    "binutils-2.43.1-r3": _apk(
        [("usr", "d", None, 0o755), ("usr/bin", "d", None, 0o755)]
        + [("usr/bin/strings", "f", b"binutils strings", 0o755)]
    ),
    "hello-1.0-r0": _apk(DATA[:6]),
}


def _sha(stem: str) -> str:
    return hashlib.sha256(FIXTURE_APKS[stem]).hexdigest()


SCRIPTED_APK = {
    "scripted-1.0-r0": _apk(
        [("etc", "d", None, 0o755), ("etc/scripted.conf", "f", b"x", 0o644)],
        scripts={".post-install": b"#!/bin/sh\ntouch /etc/post-install-ran\n"},
    )
}


def _fixture_lock(apk_dir: Path, minirootfs: str = "3.21.10", apks=FIXTURE_APKS) -> str:
    apk_dir.mkdir(parents=True, exist_ok=True)
    lines = [f"# minirootfs {minirootfs}"]
    for stem, data in sorted(apks.items()):
        (apk_dir / f"{stem}.apk").write_bytes(data)
        name, version = re.fullmatch(r"(.+?)-(\d.*)", stem).groups()
        lines.append(f"{name} {version} main {hashlib.sha256(data).hexdigest()}")
    return "\n".join(lines) + "\n"


def test_stage_extracts_checks_and_reports(tmp_path):
    mini, apks, dest = tmp_path / "mini", tmp_path / "apks", tmp_path / "stage"
    _stub_minirootfs(mini)
    shutil.copytree(mini, dest, symlinks=True)
    lock = al.parse_lock(_fixture_lock(apks))
    staged = al.stage(lock, apks, mini, dest)
    assert staged.handled == ()
    assert staged.replaced == ("usr/bin/strings",)
    assert (dest / "usr/bin/strings").read_bytes() == b"binutils strings"
    assert (dest / "usr/bin/hello").read_bytes() == HELLO


def test_stage_fills_a_directory_an_earlier_package_made_read_only(tmp_path):
    """Modes are applied once every package is in: a's 0555 directory
    still takes b's file, and ends 0555."""
    mini, apks, dest = tmp_path / "mini", tmp_path / "apks", tmp_path / "stage"
    _stub_minirootfs(mini)
    shutil.copytree(mini, dest, symlinks=True)
    two = {
        "a-1.0-r0": _apk(
            [("usr/share/ro", "d", None, 0o555), ("usr/share/ro/a", "f", b"a", 0o644)]
        ),
        "b-1.0-r0": _apk(
            [("usr/share/ro", "d", None, 0o555), ("usr/share/ro/b", "f", b"b", 0o644)]
        ),
    }
    al.stage(al.parse_lock(_fixture_lock(apks, apks=two)), apks, mini, dest)
    assert (dest / "usr/share/ro/b").read_bytes() == b"b"
    assert (dest / "usr/share/ro").stat().st_mode & 0o7777 == 0o555
    (dest / "usr/share/ro").chmod(0o755)


def test_stage_refuses_two_packages_disagreeing_on_a_directorys_mode(tmp_path):
    """Two packages giving one directory different modes is an error, not
    the later name's win; a minirootfs directory takes the package's mode."""
    mini, apks, dest = tmp_path / "mini", tmp_path / "apks", tmp_path / "stage"
    _stub_minirootfs(mini)
    (mini / "etc").chmod(0o775)  # as tools/fetch-deps.sh leaves it under umask 002
    shutil.copytree(mini, dest, symlinks=True)
    agree = {"a-1.0-r0": _apk([("etc", "d", None, 0o755), ("etc/a", "f", b"a", 0o644)])}
    al.stage(al.parse_lock(_fixture_lock(apks, apks=agree)), apks, mini, dest)
    assert (dest / "etc").stat().st_mode & 0o7777 == 0o755
    two = {
        "a-1.0-r0": _apk([("usr/share/x", "d", None, 0o755)]),
        "b-1.0-r0": _apk([("usr/share/x", "d", None, 0o700)]),
    }
    dest = tmp_path / "stage2"
    shutil.copytree(mini, dest, symlinks=True)
    with pytest.raises(al.LockError, match="usr/share/x/: mode 0700, but a package gave it 0755"):
        al.stage(
            al.parse_lock(_fixture_lock(tmp_path / "apks2", apks=two)),
            tmp_path / "apks2",
            mini,
            dest,
        )


@pytest.mark.parametrize(
    ("triggers", "why"),
    [
        ("Q1other= /bin\n", "a trigger the image doesn't reproduce (Q1other=: /bin)"),
        (BUSYBOX_TRIGGER.replace(" /sbin", ""), "a trigger the image doesn't reproduce (busybox:"),
    ],
)
def test_stage_refuses_a_minirootfs_trigger_it_doesnt_know(tmp_path, triggers, why):
    mini, apks, dest = tmp_path / "mini", tmp_path / "apks", tmp_path / "stage"
    _stub_minirootfs(mini)
    (mini / "lib/apk/db/triggers").write_text(triggers)
    shutil.copytree(mini, dest, symlinks=True)
    with pytest.raises(al.LockError, match=re.escape(why)):
        al.stage(al.parse_lock(_fixture_lock(apks)), apks, mini, dest)


@pytest.mark.parametrize(
    ("path", "named"), [("lib/modules/6.6/x.ko", "lib/modules"), ("bin/bbsuid", "bin/bbsuid")]
)
def test_stage_refuses_what_busyboxs_trigger_would_act_on(tmp_path, path, named):
    """busybox's trigger runs depmod on a modules directory and bbsuid's
    install; mkrootfs-dev.sh reproduces neither, so such a path is refused."""
    mini, apks, dest = tmp_path / "mini", tmp_path / "apks", tmp_path / "stage"
    _stub_minirootfs(mini)
    shutil.copytree(mini, dest, symlinks=True)
    one = {"k-1.0-r0": _apk([(path, "f", b"x", 0o755)])}
    with pytest.raises(al.LockError, match=f"{named} is in the image"):
        al.stage(al.parse_lock(_fixture_lock(apks, apks=one)), apks, mini, dest)


def test_stage_refuses_two_packages_replacing_one_link(tmp_path):
    mini, apks, dest = tmp_path / "mini", tmp_path / "apks", tmp_path / "stage"
    _stub_minirootfs(mini)
    shutil.copytree(mini, dest, symlinks=True)
    two = {**FIXTURE_APKS, "other-1.0-r0": _apk([("usr/bin/strings", "f", b"other", 0o755)])}
    with pytest.raises(al.LockError, match="usr/bin/strings is already in the image"):
        al.stage(al.parse_lock(_fixture_lock(apks, apks=two)), apks, mini, dest)


@pytest.mark.parametrize("script", [".pre-install", ".trigger", ".post-upgrade"])
def test_stage_refuses_every_kind_of_install_script(tmp_path, script):
    mini, apks, dest = tmp_path / "mini", tmp_path / "apks", tmp_path / "stage"
    _stub_minirootfs(mini)
    shutil.copytree(mini, dest, symlinks=True)
    apk = {"s-1.0-r0": _apk([("etc", "d", None, 0o755)], scripts={script: b"#!/bin/sh\n"})}
    with pytest.raises(al.LockError, match=f"s carries {script}"):
        al.stage(al.parse_lock(_fixture_lock(apks, apks=apk)), apks, mini, dest)


def test_stage_refuses_install_scripts_handled_scripts_doesnt_list(tmp_path, monkeypatch):
    """A package carrying a script fails before anything is extracted,
    naming the script; listed in HANDLED_SCRIPTS, it passes, the script
    still never run, and stage reports how it is reproduced."""
    mini, apks = tmp_path / "mini", tmp_path / "apks"
    _stub_minirootfs(mini)
    lock = al.parse_lock(_fixture_lock(apks, apks={**FIXTURE_APKS, **SCRIPTED_APK}))
    dest = tmp_path / "s0"
    shutil.copytree(mini, dest, symlinks=True)
    with pytest.raises(al.LockError, match="scripted carries .post-install.*HANDLED_SCRIPTS"):
        al.stage(lock, apks, mini, dest)
    assert not (dest / "usr/bin/hello").exists()
    # Listed for other content (a bump that changed the script): still refused.
    monkeypatch.setitem(al.HANDLED_SCRIPTS, ("scripted", ".post-install", "0" * 64), "old")
    with pytest.raises(al.LockError, match="scripted carries .post-install"):
        al.stage(lock, apks, mini, dest)
    sha = hashlib.sha256(b"#!/bin/sh\ntouch /etc/post-install-ran\n").hexdigest()
    monkeypatch.setitem(al.HANDLED_SCRIPTS, ("scripted", ".post-install", sha), "nothing to do")
    dest = tmp_path / "s1"
    shutil.copytree(mini, dest, symlinks=True)
    staged = al.stage(lock, apks, mini, dest)
    assert staged.handled == (("scripted", ".post-install", "nothing to do"),)
    assert (dest / "etc/scripted.conf").read_bytes() == b"x"
    assert not (dest / "etc/post-install-ran").exists()


def _built_or_skip(what: Path, ready: bool, make: str) -> None:
    """Skips a test of what `make` leaves in build/ when it isn't there:
    locally only. CI builds it before the tests, so there a missing one is a
    failure, not a silent skip."""
    if ready:
        return
    why = f"{what} isn't built (`{make}`)"
    if os.environ.get("CI"):
        pytest.fail(why)
    pytest.skip(why)


def test_the_locked_packages_carry_no_unhandled_install_scripts():
    """Every package in tools/alpine-packages.lock that build/deps holds
    carries only scripts HANDLED_SCRIPTS lists (none at #344's lock), so
    the image needs nothing a script would have done. Skipped locally
    without the fetched packages, failed under CI (which fetches them)."""
    apk_dir = BUILD_DIR / "deps/alpine-packages"
    lock = al.parse_lock(LOCK.read_text())
    ready = all((apk_dir / p.filename).is_file() for p in lock.packages)
    _built_or_skip(apk_dir, ready, "make deps-dev")
    for pkg in lock.packages:
        data = (apk_dir / pkg.filename).read_bytes()
        assert hashlib.sha256(data).hexdigest() == pkg.sha256, pkg.filename
        for script in al.split_apk(data).scripts:
            assert (pkg.name, *script) in al.HANDLED_SCRIPTS, (pkg.name, script)


def test_stage_refusals(tmp_path):
    mini, apks = tmp_path / "mini", tmp_path / "apks"
    _stub_minirootfs(mini)
    lock = al.parse_lock(_fixture_lock(apks))
    with pytest.raises(al.LockError, match="resolved against minirootfs 3.21.9.*has 3.21.10"):
        al.stage(al.parse_lock(_fixture_lock(apks, "3.21.9")), apks, mini, tmp_path / "s0")
    (apks / "hello-1.0-r0.apk").write_bytes(b"tampered")
    with pytest.raises(al.LockError, match="hello-1.0-r0.apk: sha256 .* run `make deps-dev`"):
        al.stage(lock, apks, mini, tmp_path / "s1")
    (apks / "hello-1.0-r0.apk").unlink()
    with pytest.raises(al.LockError, match="hello-1.0-r0.apk is missing"):
        al.stage(lock, apks, mini, tmp_path / "s2")


def test_image_size_adds_a_quarter_of_the_inodes(tmp_path, monkeypatch):
    for i in range(20):
        (tmp_path / f"f{i}").write_bytes(b"")
    monkeypatch.setattr(al, "INODE_HEADROOM", 1)
    s = al.image_size(tmp_path)
    assert s.inodes == 21 + 21 // 4


def test_image_size_refuses_a_missing_stage(tmp_path):
    with pytest.raises(al.LockError, match="no staged tree at"):
        al.image_size(tmp_path / "missing")


def _index_blob(stanzas: str) -> bytes:
    """An APKINDEX.tar.gz: its APKINDEX and DESCRIPTION."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        for name, body in (("DESCRIPTION", b"v3.21.0-fixture\n"), ("APKINDEX", stanzas.encode())):
            info = tarfile.TarInfo(name)
            info.size = len(body)
            tar.addfile(info, io.BytesIO(body))
    return buf.getvalue()


def _stanza(stem: str, data: bytes, *, checksum=None, size=None) -> str:
    name, version = re.fullmatch(r"(.+?)-(\d.*)", stem).groups()
    lines = [f"P:{name}", f"V:{version}"]
    if checksum is not False:
        lines.append(f"C:{checksum or al.control_checksum(data)}")
    if size is not False:
        lines.append(f"S:{len(data) if size is None else size}")
    return "\n".join(lines) + "\n"


def _lock_tree(tmp_path: Path, stanzas: str):
    """A file:// mirror holding the fixture packages and an index of
    `stanzas`, a stub minirootfs, and alpine-lock.py's arguments over them."""
    mirror = tmp_path / "mirror"
    repo_dir = mirror / "v3.21/main/x86_64"
    repo_dir.mkdir(parents=True)
    for stem, data in FIXTURE_APKS.items():
        (repo_dir / f"{stem}.apk").write_bytes(data)
    (repo_dir / "APKINDEX.tar.gz").write_bytes(_index_blob(stanzas))
    mini = tmp_path / "mini"
    _stub_minirootfs(mini)
    wanted = tmp_path / "packages.txt"
    wanted.write_text("hello\nbinutils\n")
    lock = tmp_path / "out.lock"
    args = ["lock", "--packages", str(wanted), "--lock", str(lock), "--minirootfs", str(mini)]
    args += ["--apk-dir", str(tmp_path / "apks"), "--mirror", f"file://{mirror}", "--repo", "main"]
    return args, lock, repo_dir


def test_lock_resolves_downloads_checks_and_writes_the_lock(tmp_path):
    """`lock` end to end over a file:// mirror: each .apk checked against
    the index's S: and C:, its sha256 recorded, the index's description and
    sha256 in the header; the lock parses and `stage` takes it."""
    stanzas = "\n".join(_stanza(stem, data) for stem, data in sorted(FIXTURE_APKS.items()))
    args, lock, repo_dir = _lock_tree(tmp_path, stanzas)
    assert al.main(args) == 0
    text = lock.read_text()
    index_sha = hashlib.sha256((repo_dir / "APKINDEX.tar.gz").read_bytes()).hexdigest()
    assert f"#   main/x86_64/APKINDEX.tar.gz v3.21.0-fixture sha256 {index_sha}" in text
    assert "# minirootfs' 2 packages, resolved from:" in text
    parsed = al.parse_lock(text)
    assert parsed.minirootfs == "3.21.10"
    assert [(p.name, p.version, p.repo, p.sha256) for p in parsed.packages] == [
        (
            "binutils",
            "2.43.1-r3",
            "main",
            hashlib.sha256(FIXTURE_APKS["binutils-2.43.1-r3"]).hexdigest(),
        ),
        ("hello", "1.0-r0", "main", hashlib.sha256(FIXTURE_APKS["hello-1.0-r0"]).hexdigest()),
    ]
    for stem, data in FIXTURE_APKS.items():
        assert (tmp_path / "apks" / f"{stem}.apk").read_bytes() == data
    dest = tmp_path / "stage"
    shutil.copytree(tmp_path / "mini", dest, symlinks=True)
    al.stage(parsed, tmp_path / "apks", tmp_path / "mini", dest)
    assert (dest / "usr/bin/hello").read_bytes() == HELLO
    # Run again, the downloads kept: the same lock.
    assert al.main(args) == 0
    assert lock.read_text() == text


@pytest.mark.parametrize(
    ("override", "why"),
    [
        (
            {"checksum": "Q1AAAAAAAAAAAAAAAAAAAAAAAAAAAA="},
            "control segment doesn't match the index's C:",
        ),
        ({"size": 3}, "bytes, the index says 3"),
        ({"checksum": False}, "the index gives hello no C: or S:"),
        ({"size": False}, "the index gives hello no C: or S:"),
    ],
)
def test_lock_refuses_a_file_the_index_doesnt_vouch_for(tmp_path, capsys, override, why):
    stanzas = _stanza("binutils-2.43.1-r3", FIXTURE_APKS["binutils-2.43.1-r3"]) + "\n"
    stanzas += _stanza("hello-1.0-r0", FIXTURE_APKS["hello-1.0-r0"], **override)
    args, lock, _ = _lock_tree(tmp_path, stanzas)
    assert al.main(args) == 1
    assert why in capsys.readouterr().err
    assert not lock.exists()


def test_lock_refuses_a_data_segment_its_control_segment_doesnt_vouch_for(tmp_path, capsys):
    """Codex's case on #366: the mirror's hello has the indexed size and
    C:, but other data. And a package whose .PKGINFO gives no datahash
    can't be locked."""
    real = FIXTURE_APKS["hello-1.0-r0"]
    stanzas = _stanza("binutils-2.43.1-r3", FIXTURE_APKS["binutils-2.43.1-r3"]) + "\n"
    stanzas += _stanza("hello-1.0-r0", real)
    args, lock, repo_dir = _lock_tree(tmp_path, stanzas)
    # Another payload of the same compressed size: one letter changed.
    for letter in b"abcdefgijklmnopqrstuvwxyz":
        payload = HELLO.replace(b"h", bytes([letter]))
        other = _apk([(n, k, payload if p == HELLO else p, m) for n, k, p, m in DATA[:6]])
        swapped = _swap_data(real, other)
        if len(swapped) == len(real):
            break
    assert len(swapped) == len(real)
    (repo_dir / "hello-1.0-r0.apk").write_bytes(swapped)
    assert al.main(args) == 1
    assert "hello-1.0-r0.apk: the .apk's data segment doesn't match its .PKGINFO datahash" in (
        capsys.readouterr().err
    )
    assert not lock.exists()

    bare = _apk(DATA[:6], datahash=False)
    stanzas = _stanza("binutils-2.43.1-r3", FIXTURE_APKS["binutils-2.43.1-r3"]) + "\n"
    stanzas += _stanza("hello-1.0-r0", bare)
    args, lock, repo_dir = _lock_tree(tmp_path / "bare", stanzas)
    (repo_dir / "hello-1.0-r0.apk").write_bytes(bare)
    assert al.main(args) == 1
    assert "hello-1.0-r0.apk: its .PKGINFO gives no datahash" in capsys.readouterr().err


def test_lock_rechecks_a_kept_download(tmp_path, capsys):
    stanzas = "\n".join(_stanza(stem, data) for stem, data in sorted(FIXTURE_APKS.items()))
    args, _, _ = _lock_tree(tmp_path, stanzas)
    assert al.main(args) == 0
    (tmp_path / "apks/hello-1.0-r0.apk").write_bytes(b"corrupt")
    assert al.main(args) == 1
    assert "bytes, the index says" in capsys.readouterr().err


def test_lock_lets_main_hide_community_and_names_only_contributing_repos(tmp_path):
    stanzas = "\n".join(_stanza(stem, data) for stem, data in sorted(FIXTURE_APKS.items()))
    args, lock, repo_dir = _lock_tree(tmp_path, stanzas)
    comm = repo_dir.parent.parent / "community/x86_64"
    comm.mkdir(parents=True)
    other = _apk([("usr/bin/hello", "f", b"community hello", 0o755)])
    (comm / "hello-9.0-r0.apk").write_bytes(other)
    (comm / "APKINDEX.tar.gz").write_bytes(_index_blob(_stanza("hello-9.0-r0", other)))
    args = [a for a in args if a not in ("--repo", "main")]
    assert al.main(args) == 0
    text = lock.read_text()
    assert "hello 1.0-r0 main" in text
    assert "community/x86_64/APKINDEX" not in text


def test_image_size_counts_the_same_on_every_host(tmp_path):
    (tmp_path / "a").write_bytes(b"x")
    (tmp_path / "b").write_bytes(b"x" * 4097)
    os.link(tmp_path / "a", tmp_path / "a-again")
    (tmp_path / "d").mkdir()
    (tmp_path / "short").symlink_to("a")
    (tmp_path / "long").symlink_to("x" * 60)
    s = al.image_size(tmp_path)
    content = 4096 + 8192 + 4096 + 4096 + 4096  # a, b, d, long, the root
    assert (s.content, s.inodes_used) == (content, 6)
    assert s.inodes == 6 + 4096
    assert s.size_mib == -(-(content + 64 * 2**20 + s.inodes * 256) // 2**20)


# --- tools/fetch-alpine-packages.sh ------------------------------------------------------


def _fetch_tree(tmp_path: Path):
    """A stub repository for fetch-alpine-packages.sh: the script, a lock
    of the fixture packages, a dl-cdn stand-in laid out as Alpine's mirrors
    (holding both) and an empty flat mirror."""
    tools = tmp_path / "repo/tools"
    tools.mkdir(parents=True)
    shutil.copy2(REPO_ROOT / "tools/fetch-alpine-packages.sh", tools)
    shutil.copy2(REPO_ROOT / "tools/alpine-lock.py", tools)
    src = tmp_path / "src"
    (tools / "alpine-packages.lock").write_text(_fixture_lock(src))
    cdn = tmp_path / "cdn/v3.21/main/x86_64"
    cdn.mkdir(parents=True)
    for apk in src.iterdir():
        shutil.copy(apk, cdn)
    flat = tmp_path / "flat"
    flat.mkdir()
    return tools / "fetch-alpine-packages.sh", tmp_path / "repo/build/deps/alpine-packages", flat


CURL_STUB = """#!/bin/sh
# The tests' curl: every call's arguments logged to `args`; the real curl
# (-q first, so no ~/.curlrc applies) for file:// and 127.0.0.1 URLs; any
# other URL logged to `urls` and answered as the file `answer` says (the
# HTTP code curl prints, then its exit), by default `404 22`, curl -f's on a
# 404: no test reaches the network.
here=$(dirname "$0")
echo "$*" >> "$here/args"
for a in "$@"; do url=$a; done
case "$url" in
file://* | http://127.0.0.1:*) exec {real} -q "$@" ;;
esac
echo "$url" >> "$here/urls"
set -- $(cat "$here/answer" 2>/dev/null || echo 404 22)
printf %s "$1"
exit "$2"
"""

# Taken out of every run's environment: a proxy would send even the
# loopback HTTP mirrors elsewhere.
PROXY_VARS = (
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
)


def _install_curl_stub(bin_dir: Path, answer: str | None = None) -> None:
    bin_dir.mkdir(exist_ok=True)
    real = shutil.which("curl")
    assert real, "no curl on PATH"
    (bin_dir / "curl").write_text(CURL_STUB.format(real=real))
    (bin_dir / "curl").chmod(0o755)
    if answer is not None:
        (bin_dir / "answer").write_text(answer)


def _run_fetch(
    script: Path,
    tmp_path: Path,
    mirror: str = "",
    *,
    cdn: str | None = None,
    fallback: str | None = "",
    unset: tuple[str, ...] = (),
    answer: str | None = None,
    env_extra: dict[str, str] | None = None,
):
    """Runs the fetch offline: `cdn` defaults to the tree's stand-in, a
    `fallback` of None leaves LUMEN_APK_FALLBACK unset (the real durable
    mirror, which only CURL_STUB answers, as `answer` says), and `unset`
    takes variables out."""
    bin_dir = tmp_path / "bin"
    _install_curl_stub(bin_dir, answer)
    env = {
        **os.environ,
        "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
        "LUMEN_APK_CDN": f"file://{tmp_path}/cdn" if cdn is None else cdn,
        "LUMEN_APK_MIRROR": mirror,
        "LUMEN_PYTHON": sys.executable,
        **(env_extra or {}),
    }
    if fallback is None:
        env.pop("LUMEN_APK_FALLBACK", None)
    else:
        env["LUMEN_APK_FALLBACK"] = fallback
    for name in (*unset, *PROXY_VARS):
        env.pop(name, None)
    return subprocess.run(
        ["bash", str(script)], capture_output=True, text=True, check=False, env=env, timeout=120
    )


def _put_fixtures(directory: Path, *stems: str, by_sha: bool = False) -> None:
    """The fixture packages (all, or `stems`) in a flat directory, each named
    by its file name, or with `by_sha` by its sha256."""
    for stem in stems or FIXTURE_APKS:
        name = f"{_sha(stem)}.apk" if by_sha else f"{stem}.apk"
        (directory / name).write_bytes(FIXTURE_APKS[stem])


def _assert_fetched(out: Path) -> None:
    for stem, data in FIXTURE_APKS.items():
        assert (out / f"{stem}.apk").read_bytes() == data, stem


def _curl_args(tmp_path: Path) -> list[str]:
    """Each curl call's arguments, as CURL_STUB logged them."""
    log = tmp_path / "bin/args"
    return log.read_text().splitlines() if log.exists() else []


def _offline_urls(tmp_path: Path) -> list[str]:
    """The URLs CURL_STUB answered instead of the network."""
    log = tmp_path / "bin/urls"
    return log.read_text().splitlines() if log.exists() else []


def _tried(stderr: str) -> list[tuple[str, str]]:
    """A failed fetch's `tried:` lines, as (URL, what it answered)."""
    lines = stderr.split("; tried:\n", 1)[1].splitlines()
    tried = []
    for line in lines:
        m = re.fullmatch(r"  ([a-z]+://\S+): (.+)", line)
        if not m:
            break
        tried.append((m[1], m[2]))
    return tried


def _summary(fetched: int, fallback: int, kept: int, removed: int) -> str:
    return (
        f"{fetched + kept} packages ({fetched} fetched ({fallback} from the fallback), "
        f"{kept} already there, {removed} stale removed)"
    )


def test_fetch_tries_mirrors_in_order_and_checks_each_sha256(tmp_path):
    script, out, flat = _fetch_tree(tmp_path)
    # The flat mirror has hello right and binutils wrong: hello comes from
    # it, binutils from the CDN after the mismatch.
    (flat / "hello-1.0-r0.apk").write_bytes(FIXTURE_APKS["hello-1.0-r0"])
    (flat / "binutils-2.43.1-r3.apk").write_bytes(b"not binutils")
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{file}}")
    assert proc.returncode == 0, proc.stderr
    other = hashlib.sha256(b"not binutils").hexdigest()
    assert (
        f"file://{flat}/binutils-2.43.1-r3.apk: other bytes (sha256 {other}), trying the next source"
        in proc.stderr
    )
    assert _summary(2, 0, 0, 0) in proc.stdout
    _assert_fetched(out)


def test_fetch_keeps_verified_files_refetches_bad_ones_and_prunes_stale_ones(tmp_path):
    script, out, _ = _fetch_tree(tmp_path)
    assert _run_fetch(script, tmp_path).returncode == 0
    (out / "hello-1.0-r0.apk").write_bytes(b"corrupt")
    (out / "old-0.1-r0.apk").write_bytes(b"stale")
    (out / "notes.txt").write_text("not an .apk: kept")
    proc = _run_fetch(script, tmp_path)
    assert proc.returncode == 0, proc.stderr
    assert _summary(1, 0, 1, 1) in proc.stdout
    assert (out / "hello-1.0-r0.apk").read_bytes() == FIXTURE_APKS["hello-1.0-r0"]
    assert sorted(p.name for p in out.iterdir()) == [
        "binutils-2.43.1-r3.apk",
        "hello-1.0-r0.apk",
        "notes.txt",
    ]


def test_fetch_fails_clearly_when_a_pinned_file_is_gone_everywhere(tmp_path):
    """Every source answers not found: each tried URL with its answer, the
    advice to re-lock (the CDN answered not found), and (`_run_fetch`'s
    default, an empty fallback) that the durable mirror wasn't tried."""
    script, out, flat = _fetch_tree(tmp_path)
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{file}} file://{tmp_path}/nowhere")
    assert proc.returncode == 1
    assert "no mirror gave hello-1.0-r0.apk with sha256" in proc.stderr
    assert _tried(proc.stderr) == [
        (f"file://{flat}/hello-1.0-r0.apk", "not found (no such file)"),
        (
            f"file://{tmp_path}/nowhere/v3.21/main/x86_64/hello-1.0-r0.apk",
            "not found (no such file)",
        ),
        (f"file://{tmp_path}/cdn/v3.21/main/x86_64/hello-1.0-r0.apk", "not found (no such file)"),
    ]
    assert "Re-run tools/alpine-lock.py" in proc.stderr
    assert "LUMEN_APK_FALLBACK is set empty, so the durable mirror wasn't tried" in proc.stderr
    assert "couldn't be reached" not in proc.stderr
    assert "served other bytes" not in proc.stderr
    assert "The durable mirror should hold" not in proc.stderr
    assert "LUMEN_APK_CDN is set empty" not in proc.stderr
    assert not (out / "hello-1.0-r0.apk").exists()
    assert not [p for p in out.iterdir() if p.name.startswith(".part")]


class _Mirror(http.server.BaseHTTPRequestHandler):
    """An HTTP mirror for the fetch tests: `status` for every path, with
    `body` when it is 200."""

    status = 404
    body = b""

    def do_GET(self):
        self.send_response(self.status)
        self.send_header("Content-Length", str(len(self.body) if self.status == 200 else 0))
        self.end_headers()
        if self.status == 200:
            self.wfile.write(self.body)

    def log_message(self, *args):
        pass


@contextlib.contextmanager
def _http_mirror(status: int, body: bytes = b""):
    handler = type("Handler", (_Mirror,), {"status": status, "body": body})
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()


PORTAL = b"<html>captive portal</html>"


@pytest.mark.parametrize(
    ("status", "body", "outcome", "advice", "not_advice"),
    [
        (404, b"", "not found (HTTP 404)", "Re-run tools/alpine-lock.py", "couldn't be reached"),
        (403, b"", "not found (HTTP 403)", "Re-run tools/alpine-lock.py", "couldn't be reached"),
        (410, b"", "not found (HTTP 410)", "Re-run tools/alpine-lock.py", "couldn't be reached"),
        (503, b"", "unreachable (HTTP 503)", "couldn't be reached", "Re-run tools/alpine-lock.py"),
        (
            200,
            PORTAL,
            f"other bytes (sha256 {hashlib.sha256(PORTAL).hexdigest()})",
            "served other bytes",
            "Re-run tools/alpine-lock.py",
        ),
    ],
)
def test_fetch_tells_a_dropped_file_from_a_broken_mirror_over_http(
    tmp_path, status, body, outcome, advice, not_advice
):
    """Over HTTP, as dl-cdn answers: a 403, 404 or 410 everywhere means the
    file is gone (re-lock); a 5xx means the mirror is down, and other bytes
    under the name a proxy or corrupt mirror: neither is a reason to
    re-lock. The tried line says which."""
    script, out, _ = _fetch_tree(tmp_path)
    with _http_mirror(status, body) as base:
        proc = _run_fetch(script, tmp_path, cdn=base)
    assert proc.returncode == 1
    assert _tried(proc.stderr) == [(f"{base}/v3.21/main/x86_64/binutils-2.43.1-r3.apk", outcome)]
    assert advice in proc.stderr
    assert not_advice not in proc.stderr
    assert not list(out.glob("*.apk"))


def _serve_dir(root: Path, redirect: dict[str, str] | None = None):
    class H(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **k):
            super().__init__(*a, directory=str(root), **k)

        def log_message(self, *a):
            pass

        def do_GET(self):
            if redirect and self.path in redirect:
                self.send_response(302)
                self.send_header("Location", redirect[self.path])
                self.end_headers()
                return
            super().do_GET()

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


def test_fetch_follows_a_mirrors_redirect(tmp_path):
    script, out, flat = _fetch_tree(tmp_path)
    shutil.rmtree(tmp_path / "cdn")
    _put_fixtures(flat)
    redirect = {f"/r/{s}.apk": f"/{s}.apk" for s in FIXTURE_APKS}
    srv = _serve_dir(flat, redirect)
    try:
        proc = _run_fetch(
            script,
            tmp_path,
            f"http://127.0.0.1:{srv.server_port}/r/{{file}}",
            cdn=f"file://{tmp_path}/none",
        )
    finally:
        srv.shutdown()
        srv.server_close()
    assert proc.returncode == 0, proc.stderr
    _assert_fetched(out)


def test_fetch_other_bytes_everywhere_fail_without_a_part_file(tmp_path):
    script, out, _ = _fetch_tree(tmp_path)
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").write_bytes(b"other bytes")
    proc = _run_fetch(script, tmp_path)
    assert proc.returncode == 1
    assert "served other bytes" in proc.stderr
    assert "Re-run tools/alpine-lock.py" not in proc.stderr
    assert "couldn't be reached" not in proc.stderr
    assert not [p for p in out.iterdir() if p.name.startswith(".part")]


def test_fetch_prints_every_advice_line_that_applies(tmp_path):
    """One source serving other bytes, the next unreachable: both lines of
    advice, each from its own answer, and no re-lock."""
    script, _, flat = _fetch_tree(tmp_path)
    (flat / "binutils-2.43.1-r3.apk").write_bytes(b"not binutils")
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{file}}", cdn="http://127.0.0.1:9")
    assert proc.returncode == 1
    assert [o for _, o in _tried(proc.stderr)] == [
        f"other bytes (sha256 {hashlib.sha256(b'not binutils').hexdigest()})",
        "unreachable (curl exit 7)",
    ]
    assert "served other bytes" in proc.stderr
    assert "couldn't be reached" in proc.stderr
    assert "Re-run tools/alpine-lock.py" not in proc.stderr


def test_fetch_stops_at_the_first_good_mirror(tmp_path):
    script, out, flat = _fetch_tree(tmp_path)
    _put_fixtures(flat)
    # The CDN, tried second, gives wrong bytes: never consulted.
    for p in (tmp_path / "cdn/v3.21/main/x86_64").iterdir():
        p.write_bytes(b"wrong")
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{file}}")
    assert proc.returncode == 0, proc.stderr
    assert "sha256" not in proc.stderr
    assert "trying the next source" not in proc.stderr
    _assert_fetched(out)


def test_fetch_reads_mirrors_one_per_line(tmp_path):
    """LUMEN_APK_MIRROR split on newlines too (a YAML block): the second
    line's mirror is used, not dropped."""
    script, out, flat = _fetch_tree(tmp_path)
    shutil.rmtree(tmp_path / "cdn")
    _put_fixtures(flat)
    proc = _run_fetch(
        script, tmp_path, f"file://{tmp_path}/nowhere/{{file}}\nfile://{flat}/{{file}}"
    )
    assert proc.returncode == 0, proc.stderr
    _assert_fetched(out)


def test_fetch_says_an_unreachable_mirror_is_no_reason_to_relock(tmp_path):
    """A mirror that can't be reached (nothing listens on port 9) fails the
    fetch with "check the network", not the advice to re-run the lock."""
    script, _, _ = _fetch_tree(tmp_path)
    proc = _run_fetch(script, tmp_path, cdn="http://127.0.0.1:9")
    assert proc.returncode == 1
    assert _tried(proc.stderr) == [
        ("http://127.0.0.1:9/v3.21/main/x86_64/binutils-2.43.1-r3.apk", "unreachable (curl exit 7)")
    ]
    assert "couldn't be reached" in proc.stderr
    assert "Re-run tools/alpine-lock.py" not in proc.stderr


def test_fetch_takes_a_sha256_template_and_refuses_other_braces(tmp_path):
    script, out, flat = _fetch_tree(tmp_path)
    shutil.rmtree(tmp_path / "cdn")
    _put_fixtures(flat, by_sha=True)
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{sha256}}.apk")
    assert proc.returncode == 0, proc.stderr
    _assert_fetched(out)
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{name}}.apk")
    assert proc.returncode == 1
    assert "a template takes only {file} and {sha256}" in proc.stderr


@pytest.mark.parametrize(
    ("edit", "why"),
    [
        (lambda t: t + "evil;rm 1-r0 main " + "a" * 64 + "\n", "bad name or version 'evil;rm'"),
        (lambda t: t.replace(" main ", " testing ", 1), "unknown repository 'testing'"),
        (lambda t: t[:-2] + "\n", "bad sha256"),
    ],
)
def test_fetch_reads_the_lock_through_alpine_lock(tmp_path, edit, why):
    """The fetch script takes the lock from `alpine-lock.py files`, so a
    line parse_lock refuses fails the fetch before any download."""
    script, out, _ = _fetch_tree(tmp_path)
    lock = script.parent / "alpine-packages.lock"
    lock.write_text(edit(lock.read_text()))
    proc = _run_fetch(script, tmp_path)
    assert proc.returncode != 0
    assert why in proc.stderr
    assert "no mirror gave" not in proc.stderr  # stopped before any download
    assert not out.exists() or not list(out.glob("*.apk"))


def test_fetch_keeps_a_last_lock_line_without_a_newline(tmp_path):
    script, out, _ = _fetch_tree(tmp_path)
    lock = script.parent / "alpine-packages.lock"
    lock.write_text(lock.read_text().rstrip("\n"))
    proc = _run_fetch(script, tmp_path)
    assert proc.returncode == 0, proc.stderr
    assert _summary(2, 0, 0, 0) in proc.stdout
    assert sorted(p.name for p in out.glob("*.apk")) == sorted(f"{s}.apk" for s in FIXTURE_APKS)


def test_fetch_takes_a_community_package_from_its_repository(tmp_path):
    script, out, _ = _fetch_tree(tmp_path)
    lock = script.parent / "alpine-packages.lock"
    lock.write_text(lock.read_text().replace(" main ", " community ", 1))
    community = tmp_path / "cdn/v3.21/community/x86_64"
    community.mkdir(parents=True)
    (tmp_path / "cdn/v3.21/main/x86_64/binutils-2.43.1-r3.apk").rename(
        community / "binutils-2.43.1-r3.apk"
    )
    proc = _run_fetch(script, tmp_path)
    assert proc.returncode == 0, proc.stderr
    assert (out / "binutils-2.43.1-r3.apk").read_bytes() == FIXTURE_APKS["binutils-2.43.1-r3"]


def test_files_prints_the_locks_mirror_path_file_and_sha256(tmp_path, capsys):
    lock = tmp_path / "lock"
    lock.write_text(_fixture_lock(tmp_path / "apks"))
    assert al.main(["files", "--lock", str(lock)]) == 0
    want = [
        f"v3.21/main/x86_64/{stem}.apk {stem}.apk {_sha(stem)}" for stem in sorted(FIXTURE_APKS)
    ]
    assert capsys.readouterr().out.splitlines() == want


def test_fetch_tries_the_durable_mirror_only_after_the_cdn(tmp_path):
    """LUMEN_APK_FALLBACK comes after dl-cdn: a file the CDN dropped comes
    from it, saying so, while one the CDN still has comes from the CDN (the
    fallback's wrong copy is never read)."""
    script, out, flat = _fetch_tree(tmp_path)
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    (flat / f"{_sha('hello-1.0-r0')}.apk").write_bytes(FIXTURE_APKS["hello-1.0-r0"])
    (flat / f"{_sha('binutils-2.43.1-r3')}.apk").write_bytes(b"wrong")
    proc = _run_fetch(script, tmp_path, fallback=f"file://{flat}/{{sha256}}.apk")
    assert proc.returncode == 0, proc.stderr
    assert "sha256" not in proc.stderr  # the fallback's wrong binutils never fetched
    assert (
        f"file://{tmp_path}/cdn/v3.21/main/x86_64/hello-1.0-r0.apk: not found (no such file), "
        "trying the next source" in proc.stderr
    )
    assert _summary(2, 1, 0, 0) in proc.stdout
    _assert_fetched(out)


def test_fetch_falls_back_past_an_unreachable_cdn(tmp_path):
    """dl-cdn down: every file comes from the durable mirror, and the run
    succeeds without the "couldn't be reached" advice. The dead CDN is tried
    for the first file only, then skipped for the rest of the run, so a
    stalled dl-cdn costs one file's timeouts, not every file's."""
    script, out, flat = _fetch_tree(tmp_path)
    _put_fixtures(flat, by_sha=True)
    proc = _run_fetch(
        script, tmp_path, cdn="http://127.0.0.1:9", fallback=f"file://{flat}/{{sha256}}.apk"
    )
    assert proc.returncode == 0, proc.stderr
    assert (
        "unreachable (curl exit 7) (skipped for the rest of this run), trying the next source"
        in proc.stderr
    )
    assert proc.stderr.count("127.0.0.1:9") == 1  # the second file never tries it
    assert "couldn't be reached" not in proc.stderr
    assert _summary(2, 2, 0, 0) in proc.stdout
    _assert_fetched(out)


def test_fetch_reads_the_fallback_as_a_list(tmp_path):
    """LUMEN_APK_FALLBACK split like LUMEN_APK_MIRROR: its second entry is
    tried after its first."""
    script, out, flat = _fetch_tree(tmp_path)
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    (flat / f"{_sha('hello-1.0-r0')}.apk").write_bytes(FIXTURE_APKS["hello-1.0-r0"])
    nowhere = f"file://{tmp_path}/nowhere/{{sha256}}.apk"
    proc = _run_fetch(script, tmp_path, fallback=f"{nowhere}\nfile://{flat}/{{sha256}}.apk")
    assert proc.returncode == 0, proc.stderr
    assert (
        f"file://{tmp_path}/nowhere/{_sha('hello-1.0-r0')}.apk: not found (no such file), "
        "trying the next source" in proc.stderr
    )
    assert _summary(2, 1, 0, 0) in proc.stdout
    assert (out / "hello-1.0-r0.apk").read_bytes() == FIXTURE_APKS["hello-1.0-r0"]


def test_fetch_refuses_a_bad_fallback_template(tmp_path):
    script, out, _ = _fetch_tree(tmp_path)
    proc = _run_fetch(script, tmp_path, fallback="file:///srv/apks/{name}.apk")
    assert proc.returncode == 1
    assert "mirror file:///srv/apks/{name}.apk: a template takes only {file} and {sha256}" in (
        proc.stderr
    )
    assert not out.exists() or not list(out.glob("*.apk"))


def test_fetch_empty_fallback_means_none(tmp_path):
    """LUMEN_APK_FALLBACK set but empty tries no fallback: a file the CDN
    dropped fails the fetch, naming only the CDN's URL, and says the durable
    mirror wasn't tried."""
    script, _, _ = _fetch_tree(tmp_path)
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    proc = _run_fetch(script, tmp_path, fallback="")
    assert proc.returncode == 1
    assert [u for u, _ in _tried(proc.stderr)] == [
        f"file://{tmp_path}/cdn/v3.21/main/x86_64/hello-1.0-r0.apk"
    ]
    assert "LUMEN_APK_FALLBACK is set empty, so the durable mirror wasn't tried" in proc.stderr
    assert _offline_urls(tmp_path) == []


def test_fetch_unset_fallback_tries_the_durable_mirror_last(tmp_path):
    """Unset, LUMEN_APK_FALLBACK is the real durable mirror (offline here:
    CURL_STUB answers it 404). It is tried after the CDN, and its not-found
    says the mirror should hold the file."""
    script, _, _ = _fetch_tree(tmp_path)
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    proc = _run_fetch(script, tmp_path, fallback=None)
    assert proc.returncode == 1
    release = (
        "https://github.com/ahmad-luqman/lumen-os/releases/download/alpine-apk-mirror/"
        f"{_sha('hello-1.0-r0')}.apk"
    )
    assert _tried(proc.stderr) == [
        (f"file://{tmp_path}/cdn/v3.21/main/x86_64/hello-1.0-r0.apk", "not found (no such file)"),
        (release, "not found (HTTP 404)"),
    ]
    assert _offline_urls(tmp_path) == [release]
    assert "The durable mirror should hold every locked file" in proc.stderr
    assert "tools/mirror-alpine-packages.sh upload" in proc.stderr
    assert "LUMEN_APK_FALLBACK is set empty" not in proc.stderr


def test_fetch_empty_cdn_means_none(tmp_path):
    """LUMEN_APK_CDN set but empty tries no CDN (it once meant dl-cdn), and
    says so. Only a mirror answered not found, not dl-cdn: no advice to
    re-lock."""
    script, _, flat = _fetch_tree(tmp_path)
    _put_fixtures(flat, "hello-1.0-r0")
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{file}}", cdn="")
    assert proc.returncode == 1
    assert _tried(proc.stderr) == [
        (f"file://{flat}/binutils-2.43.1-r3.apk", "not found (no such file)")
    ]
    assert _offline_urls(tmp_path) == []
    assert "LUMEN_APK_CDN is set empty, so dl-cdn wasn't tried" in proc.stderr
    assert "Re-run tools/alpine-lock.py" not in proc.stderr


def test_fetch_unset_cdn_means_dl_cdn(tmp_path):
    """Unset, LUMEN_APK_CDN is dl-cdn (offline: CURL_STUB answers it)."""
    script, _, _ = _fetch_tree(tmp_path)
    proc = _run_fetch(script, tmp_path, unset=("LUMEN_APK_CDN", "LUMEN_APK_MIRROR"))
    assert proc.returncode == 1
    url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/binutils-2.43.1-r3.apk"
    assert _tried(proc.stderr) == [(url, "not found (HTTP 404)")]
    assert _offline_urls(tmp_path) == [url]


def test_fetch_a_skipped_dead_cdn_still_counts_as_unreachable(tmp_path):
    """dl-cdn down, the fallback holding only the first file: the second
    file's failure lists the CDN as skipped and advises the network, not a
    re-lock."""
    script, _, flat = _fetch_tree(tmp_path)
    _put_fixtures(flat, "binutils-2.43.1-r3", by_sha=True)
    proc = _run_fetch(
        script, tmp_path, cdn="http://127.0.0.1:9", fallback=f"file://{flat}/{{sha256}}.apk"
    )
    assert proc.returncode == 1
    assert _tried(proc.stderr) == [
        (
            "http://127.0.0.1:9/v3.21/main/x86_64/hello-1.0-r0.apk",
            "skipped (unreachable earlier in this run)",
        ),
        (f"file://{flat}/{_sha('hello-1.0-r0')}.apk", "not found (no such file)"),
    ]
    assert "couldn't be reached" in proc.stderr
    assert "Re-run tools/alpine-lock.py" not in proc.stderr


def test_fetch_advice_comes_from_the_failing_files_answers_alone(tmp_path):
    """binutils meets other bytes on the flat mirror, then comes from the
    CDN; hello is gone everywhere: its advice is the re-lock, not the
    earlier file's mismatch."""
    script, _, flat = _fetch_tree(tmp_path)
    (flat / "binutils-2.43.1-r3.apk").write_bytes(b"not binutils")
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{file}}")
    assert proc.returncode == 1
    advice = proc.stderr.split("; tried:\n", 1)[1]
    assert "Re-run tools/alpine-lock.py" in advice
    assert "served other bytes" not in advice


def test_fetch_reads_a_404_by_its_code_whatever_curls_exit(tmp_path):
    """macOS's curl exits 56, not 22, on an HTTP/2 404: still not found, by
    the code. The mirror answering so isn't skipped for the next file, and
    the durable mirror's 404 says it should hold the file."""
    script, _, _ = _fetch_tree(tmp_path)
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    mirror = "https://mirror.example/alpine"
    proc = _run_fetch(script, tmp_path, mirror, fallback=None, answer="404 56")
    assert proc.returncode == 1
    release = (
        f"https://github.com/{MIRROR_REPO}/releases/download/{MIRROR_TAG}/"
        f"{_sha('hello-1.0-r0')}.apk"
    )
    assert _tried(proc.stderr) == [
        (f"{mirror}/v3.21/main/x86_64/hello-1.0-r0.apk", "not found (HTTP 404)"),
        (f"file://{tmp_path}/cdn/v3.21/main/x86_64/hello-1.0-r0.apk", "not found (no such file)"),
        (release, "not found (HTTP 404)"),
    ]
    assert "skipped" not in proc.stderr
    assert "couldn't be reached" not in proc.stderr
    assert "The durable mirror should hold every locked file" in proc.stderr
    assert "Re-run tools/alpine-lock.py" in proc.stderr


def test_fetch_reads_a_403_from_the_durable_mirror_as_refused(tmp_path):
    """GitHub answers a missing asset with a 404, so the durable mirror's
    403 is a refusal: unreachable, not a file to upload."""
    script, _, _ = _fetch_tree(tmp_path)
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    proc = _run_fetch(script, tmp_path, fallback=None, answer="403 22")
    assert proc.returncode == 1
    assert _tried(proc.stderr)[-1][1] == "refused (HTTP 403)"
    assert "couldn't be reached" in proc.stderr
    assert "The durable mirror should hold" not in proc.stderr


def test_fetch_stops_at_once_on_a_local_error(tmp_path):
    """curl can't write its part file (a read-only build/deps/alpine-packages,
    curl's exit 23): the fetch stops at the first source with that error,
    tries no other, and gives no network advice."""
    script, out, flat = _fetch_tree(tmp_path)
    _put_fixtures(flat, by_sha=True)
    out.mkdir(parents=True)
    out.chmod(0o555)
    try:
        proc = _run_fetch(script, tmp_path, fallback=f"file://{flat}/{{sha256}}.apk")
    finally:
        out.chmod(0o755)
    assert proc.returncode == 1
    assert f"local error (curl exit 23): can't write {out}/.part." in proc.stderr
    assert len(_curl_args(tmp_path)) == 1
    for text in ("tried:", "trying the next source", "couldn't be reached", "Re-run"):
        assert text not in proc.stderr


def test_fetch_caps_each_try_with_lumen_apk_max_time(tmp_path):
    script, _, _ = _fetch_tree(tmp_path)
    proc = _run_fetch(script, tmp_path, env_extra={"LUMEN_APK_MAX_TIME": "7"})
    assert proc.returncode == 0, proc.stderr
    args = _curl_args(tmp_path)
    assert len(args) == 2 and all(" --max-time 7 " in a for a in args)
    (tmp_path / "bin/args").unlink()
    shutil.rmtree(tmp_path / "repo/build")
    assert _run_fetch(script, tmp_path, env_extra={"LUMEN_APK_MAX_TIME": ""}).returncode == 0
    assert not any("--max-time" in a for a in _curl_args(tmp_path))
    proc = _run_fetch(script, tmp_path, env_extra={"LUMEN_APK_MAX_TIME": "5m"})
    assert proc.returncode == 1
    assert "LUMEN_APK_MAX_TIME is '5m', not a number of seconds" in proc.stderr


def test_fetch_needs_no_source_when_every_file_is_already_there(tmp_path):
    """All three variables empty: fine while every file is verified; the
    first one that needs fetching fails, saying there is no source."""
    script, out, _ = _fetch_tree(tmp_path)
    assert _run_fetch(script, tmp_path).returncode == 0
    proc = _run_fetch(script, tmp_path, cdn="")
    assert proc.returncode == 0, proc.stderr
    assert _summary(0, 0, 2, 0) in proc.stdout
    (out / "hello-1.0-r0.apk").write_bytes(b"corrupt")
    proc = _run_fetch(script, tmp_path, cdn="")
    assert proc.returncode == 1
    assert "hello-1.0-r0.apk needs fetching, but there is no source to try" in proc.stderr


def _script_value(path: Path, name: str) -> str:
    m = re.search(rf"^{name}=(.*)$", path.read_text(), re.MULTILINE)
    assert m, f"{path.name} sets no {name}"
    return m.group(1).strip("'\"")


def test_the_fetchs_durable_mirror_is_the_release_the_mirror_tool_fills():
    fetch = REPO_ROOT / "tools/fetch-alpine-packages.sh"
    tool = REPO_ROOT / "tools/mirror-alpine-packages.sh"
    repo, tag = _script_value(tool, "REPO"), _script_value(tool, "TAG")
    assert _script_value(fetch, "DURABLE_MIRROR") == (
        f"https://github.com/{repo}/releases/download/{tag}/{{sha256}}.apk"
    )


# --- tools/mirror-alpine-packages.sh -----------------------------------------------------

MIRROR_REPO = "ahmad-luqman/lumen-os"
MIRROR_TAG = "alpine-apk-mirror"

_GH_STUB = r"""#!@PYTHON@
# A stand-in for gh over a fake release in $GH_STATE: its assets are the
# files in assets/ (no such directory: no release), each one's digest the
# sha256 of its bytes, listed 2 to a page; a name in `partial` is an upload
# still open. Marker files: `draft` makes the release a draft; `view-error`
# makes every `release view` fail otherwise than "not found", and
# `view-fail-once`/`api-fail-once` the next one only; `view-garbage` gives an
# answer that isn't an id and a draft flag; `api-error` fails every listing;
# `lag` hides every asset from the first listing after an upload, and
# `view-lag` makes the first `release view` after a create say "release not
# found";
# `fail-upload` makes the next upload fail after leaving a partial asset,
# `upload-open` leaves each upload open, and `touch-source` changes the
# source file under build/deps after it was staged. Every call is logged to
# `calls`, one JSON list a line. It refuses what the script must never send:
# another tag or repository, an unexpected --jq, or --clobber.
import hashlib, json, os, shutil, sys

args = sys.argv[1:]
state = os.environ["GH_STATE"]
assets = os.path.join(state, "assets")
with open(os.path.join(state, "calls"), "a") as f:
    print(json.dumps(args), file=f)
REPO, TAG, ID = "@REPO@", "@TAG@", "4242"
VIEW_JQ = '"\\(.databaseId) \\(.isDraft)"'
ASSETS_JQ = '.[] | [.name, .state, (.digest // "")] | @tsv'


def refuse(why):
    print(f"gh stub: {why}: {args}", file=sys.stderr)
    sys.exit(3)


def opt(name):
    if name not in args:
        refuse(f"no {name}")
    return args[args.index(name) + 1]


def marked(name):
    return os.path.exists(os.path.join(state, name))


def once(name):
    if marked(name):
        os.remove(os.path.join(state, name))
        return True
    return False


def listed():
    partial = set()
    if marked("partial"):
        with open(os.path.join(state, "partial")) as f:
            partial = set(f.read().split())
    rows = []
    for name in sorted(os.listdir(assets)):
        with open(os.path.join(assets, name), "rb") as f:
            digest = "sha256:" + hashlib.sha256(f.read()).hexdigest()
        rows.append([name, "open" if name in partial else "uploaded", digest])
    return rows


if "--clobber" in args:
    refuse("--clobber")
if args[:1] == ["api"]:
    path = [a for a in args[1:] if a != "--paginate"][0]
    if path.split("?")[0] != f"repos/{REPO}/releases/{ID}/assets":
        refuse("another endpoint")
    if opt("--jq") != ASSETS_JQ:
        refuse("another --jq")
    if marked("api-error") or once("api-fail-once"):
        print("HTTP 500: Internal Server Error", file=sys.stderr)
        sys.exit(1)
    if once("lagging"):
        sys.exit(0)
    rows = listed()
    for row in rows if "--paginate" in args else rows[:2]:
        print("\t".join(row))
    sys.exit(0)
if args[:1] != ["release"] or len(args) < 3:
    refuse("unexpected call")
if args[2] != TAG:
    refuse("another tag")
if opt("--repo") != REPO:
    refuse("another repository")
if args[1] == "view":
    if marked("view-error") or once("view-fail-once"):
        print("HTTP 502: Bad Gateway", file=sys.stderr)
        sys.exit(1)
    if not os.path.isdir(assets) or once("view-hidden"):
        print("release not found", file=sys.stderr)
        sys.exit(1)
    if opt("--json") != "databaseId,isDraft" or opt("--jq") != VIEW_JQ:
        refuse("another --json or --jq")
    if marked("view-garbage"):
        print("null null")
        sys.exit(0)
    print(ID, "true" if marked("draft") else "false")
elif args[1] == "create":
    if os.path.isdir(assets):
        refuse("the release exists")
    os.makedirs(assets)
    if marked("view-lag"):
        open(os.path.join(state, "view-hidden"), "w").close()
elif args[1] == "upload":
    if not os.path.isdir(assets):
        refuse("no release")
    src, _, label = args[-1].partition("#")
    name = os.path.basename(src)
    dest = os.path.join(assets, name)
    if os.path.exists(dest):
        print(f"asset under the same name already exists: {name}", file=sys.stderr)
        sys.exit(1)
    if marked("fail-upload"):
        os.remove(os.path.join(state, "fail-upload"))
        with open(src, "rb") as f, open(dest, "wb") as out:
            out.write(f.read()[:3])
        with open(os.path.join(state, "partial"), "a") as f:
            print(name, file=f)
        print("connection reset", file=sys.stderr)
        sys.exit(1)
    if marked("touch-source"):
        with open(os.path.join(os.environ["DEPS_DIR"], label), "ab") as f:
            f.write(b"changed after staging")
    shutil.copy(src, dest)
    if marked("lag"):
        os.rename(os.path.join(state, "lag"), os.path.join(state, "lagging"))
    if marked("upload-open"):
        with open(os.path.join(state, "partial"), "a") as f:
            print(name, file=f)
    with open(os.path.join(state, "labels"), "a") as f:
        print(name, label, file=f)
else:
    refuse("unexpected call")
"""

# What stands in for the real gh on PATH: a test that reached it fails.
_GH_GUARD = """#!/bin/sh
echo "gh guard: the real gh is off limits in tests" >&2
exit 99
"""


def _mirror_tree(tmp_path: Path, *, release: bool = True, fetched: bool = True):
    """A stub repository for mirror-alpine-packages.sh: the script, a lock
    of the fixture packages, and the fetch script, whose DURABLE_MIRROR (the
    download probe's URL) is the stub's assets directory; with `fetched`,
    those packages in build/deps; with `release`, the gh stub's release
    (with no assets)."""
    tools = tmp_path / "repo/tools"
    tools.mkdir(parents=True)
    shutil.copy2(REPO_ROOT / "tools/mirror-alpine-packages.sh", tools)
    shutil.copy2(REPO_ROOT / "tools/alpine-lock.py", tools)
    (tools / "alpine-packages.lock").write_text(_fixture_lock(tmp_path / "src"))
    state = tmp_path / "gh"
    _set_durable_mirror(tools, f"file://{state}/assets/{{sha256}}.apk")
    if fetched:
        deps = tmp_path / "repo/build/deps/alpine-packages"
        deps.mkdir(parents=True)
        _put_fixtures(deps)
    state.mkdir()
    (state / "tmp").mkdir()
    if release:
        (state / "assets").mkdir()
    gh = tmp_path / "gh-stub"
    stub = _GH_STUB.replace("@PYTHON@", sys.executable)
    gh.write_text(stub.replace("@REPO@", MIRROR_REPO).replace("@TAG@", MIRROR_TAG))
    gh.chmod(0o755)
    return tools / "mirror-alpine-packages.sh", state, gh


def _set_durable_mirror(tools: Path, template: str) -> None:
    """tools/fetch-alpine-packages.sh, copied with another DURABLE_MIRROR."""
    text = (REPO_ROOT / "tools/fetch-alpine-packages.sh").read_text()
    text, n = re.subn(
        r"^DURABLE_MIRROR=.*$", f"DURABLE_MIRROR='{template}'", text, flags=re.MULTILINE
    )
    assert n == 1
    (tools / "fetch-alpine-packages.sh").write_text(text)


def _run_mirror(
    script: Path, state: Path, gh: Path | None, *args: str, env: dict[str, str] | None = None
):
    """The tool with `args`, through the gh stub (`gh` None: through
    whatever `gh` is on PATH, which is the guard) and CURL_STUB. gh's own
    token and configuration are emptied, so even a real gh would be signed
    out, and its retries don't sleep. `env` overrides entries last."""
    extra = env or {}
    guard = state.parent / "guard"
    _install_curl_stub(guard)
    (guard / "gh").write_text(_GH_GUARD)
    (guard / "gh").chmod(0o755)
    env = {
        **os.environ,
        "PATH": f"{guard}{os.pathsep}{os.environ['PATH']}",
        "GH_TOKEN": "",
        "GITHUB_TOKEN": "",
        "GH_CONFIG_DIR": str(state / "gh-config"),
        "GH_STATE": str(state),
        "LUMEN_GH_RETRY_SLEEP": "0",
        "LUMEN_PYTHON": sys.executable,
        "TMPDIR": str(state / "tmp"),
        "DEPS_DIR": str(state.parent / "repo/build/deps/alpine-packages"),
    }
    if (state.parent / "bin").is_dir():
        env["PATH"] = f"{state.parent / 'bin'}{os.pathsep}{env['PATH']}"
    for name in PROXY_VARS:
        env.pop(name, None)
    if gh is None:
        env.pop("LUMEN_GH", None)
    else:
        env["LUMEN_GH"] = str(gh)
    env.update(extra)
    return subprocess.run(
        ["bash", str(script), *args],
        capture_output=True,
        text=True,
        check=False,
        env=env,
        timeout=120,
    )


def _calls(state: Path, *prefix: str) -> list[list[str]]:
    """The gh stub's calls, or those starting with `prefix`."""
    path = state / "calls"
    calls = [json.loads(ln) for ln in path.read_text().splitlines()] if path.exists() else []
    return [c for c in calls if c[: len(prefix)] == list(prefix)]


def _writes(calls: list[list[str]]) -> list[list[str]]:
    return [c for c in calls if c[:2] in (["release", "create"], ["release", "upload"])]


def _assert_fetched_assets(state: Path) -> None:
    for stem, data in FIXTURE_APKS.items():
        assert (state / "assets" / f"{_sha(stem)}.apk").read_bytes() == data, stem


def _asset(state: Path, stem: str, data: bytes | None = None) -> Path:
    """The fixture package `stem` as the release's asset, its bytes or `data`."""
    path = state / "assets" / f"{_sha(stem)}.apk"
    path.write_bytes(FIXTURE_APKS[stem] if data is None else data)
    return path


def test_mirror_check_names_each_locked_file_the_release_lacks(tmp_path):
    script, state, gh = _mirror_tree(tmp_path, fetched=False)
    _asset(state, "hello-1.0-r0")
    (state / "assets/unrelated.apk").write_bytes(b"x")
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 1
    assert (
        f"binutils-2.43.1-r3.apk (sha256 {_sha('binutils-2.43.1-r3')}) isn't in release"
        in proc.stderr
    )
    assert "hello-1.0-r0.apk" not in proc.stderr
    assert "1 locked package(s) not mirrored, 0 broken" in proc.stderr
    _asset(state, "binutils-2.43.1-r3")
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 0, proc.stderr
    assert "holds all 2 locked packages" in proc.stdout
    calls = _calls(state)
    assert calls and _writes(calls) == []  # check never writes
    assert {c[0] if c[0] == "api" else c[1] for c in calls} == {"api", "view"}


def test_mirror_check_names_every_missing_file(tmp_path):
    script, state, gh = _mirror_tree(tmp_path, fetched=False)
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 1
    for stem in FIXTURE_APKS:
        assert f"{stem}.apk (sha256 {_sha(stem)}) isn't in release {MIRROR_TAG}" in proc.stderr
    assert "2 locked package(s) not mirrored, 0 broken" in proc.stderr


def test_mirror_check_reads_every_page_of_assets(tmp_path):
    """Five assets, two to a page, the locked ones sorted last: found only
    by reading every page."""
    script, state, gh = _mirror_tree(tmp_path, fetched=False)
    for name in ("0-a.apk", "0-b.apk", "0-c.apk"):
        (state / "assets" / name).write_bytes(b"x")
    for stem in FIXTURE_APKS:
        _asset(state, stem)
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 0, proc.stderr
    [api] = _calls(state, "api")
    assert "--paginate" in api


def test_mirror_check_fails_saying_why_without_a_release(tmp_path):
    script, state, gh = _mirror_tree(tmp_path, release=False, fetched=False)
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 1
    assert (
        f"can't list release {MIRROR_TAG} of {MIRROR_REPO}: release not found, "
        "or a draft this token can't see" in proc.stderr
    )
    assert "isn't in release" not in proc.stderr
    assert len(_calls(state, "release", "view")) == 1  # an answer, never retried
    assert _writes(_calls(state)) == []


@pytest.mark.parametrize("mode", ["check", "upload"])
def test_mirror_refuses_a_draft_release(tmp_path, mode):
    script, state, gh = _mirror_tree(tmp_path)
    (state / "draft").write_text("")
    proc = _run_mirror(script, state, gh, mode)
    assert proc.returncode == 1
    assert f"release {MIRROR_TAG} of {MIRROR_REPO} is a draft" in proc.stderr
    assert _writes(_calls(state)) == []


def test_mirror_upload_stops_on_a_gh_error_other_than_not_found(tmp_path):
    """Only "release not found" means no release: any other failure of the
    probe is printed, and nothing is created."""
    script, state, gh = _mirror_tree(tmp_path, release=False)
    (state / "view-error").write_text("")
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 1
    assert f"can't read release {MIRROR_TAG} of {MIRROR_REPO}: HTTP 502: Bad Gateway" in (
        proc.stderr
    )
    assert len(_calls(state, "release", "view")) == 3  # tried 3 times
    assert _writes(_calls(state)) == []


@pytest.mark.parametrize("mode", ["check", "upload"])
@pytest.mark.parametrize("damage", ["partial", "other bytes"])
def test_mirror_refuses_a_locked_name_that_isnt_a_finished_upload(tmp_path, mode, damage):
    """A locked file's name whose asset is still open, or whose digest isn't
    the name's sha256, counts as broken, not mirrored: an error naming it and
    the command that deletes it. `upload` writes nothing then (binutils,
    missing, isn't uploaded either), and never touches the asset."""
    script, state, gh = _mirror_tree(tmp_path)
    sha = _sha("hello-1.0-r0")
    if damage == "partial":
        asset = _asset(state, "hello-1.0-r0")
        (state / "partial").write_text(f"{sha}.apk\n")
        state_word, digest = "open", sha
    else:
        asset = _asset(state, "hello-1.0-r0", b"other bytes")
        state_word, digest = "uploaded", hashlib.sha256(b"other bytes").hexdigest()
    before = asset.read_bytes()
    proc = _run_mirror(script, state, gh, mode)
    assert proc.returncode == 1
    assert (
        f"release {MIRROR_TAG} holds {sha}.apk (hello-1.0-r0.apk), but its state is "
        f"'{state_word}' and its digest 'sha256:{digest}'" in proc.stderr
    )
    assert f"gh release delete-asset {MIRROR_TAG} {sha}.apk --repo {MIRROR_REPO}" in proc.stderr
    if mode == "check":
        assert "1 locked package(s) not mirrored, 1 broken" in proc.stderr
    else:
        assert "nothing uploaded: delete the broken asset(s) above first" in proc.stderr
    assert _writes(_calls(state)) == []
    assert asset.read_bytes() == before


def test_mirror_upload_creates_the_release_and_uploads_only_whats_missing(tmp_path):
    script, state, gh = _mirror_tree(tmp_path, release=False)
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 0, proc.stderr
    assert "2 uploaded" in proc.stdout
    assert "holds all 2 locked packages" in proc.stdout  # its closing check
    [create] = _calls(state, "release", "create")
    assert create[:-1] == [
        "release",
        "create",
        MIRROR_TAG,
        "--repo",
        MIRROR_REPO,
        "--prerelease",
        "--title",
        "Alpine packages for the developer image",
        "--notes",
    ]
    notes = create[-1]
    assert "3.21" not in notes  # each package's branch is in its lock
    assert "aports on branch X.Y-stable" in notes
    assets = state / "assets"
    _assert_fetched_assets(state)
    labels = (state / "labels").read_text().splitlines()
    assert sorted(labels) == sorted(f"{_sha(s)}.apk {s}.apk" for s in FIXTURE_APKS)
    # Each upload is a staged copy under TMPDIR, removed afterwards.
    for c in _calls(state, "release", "upload"):
        assert Path(c[-1]).parent.name.startswith("lumen-apk-mirror.")
        assert Path(c[-1]).parent.parent == state / "tmp"
    assert list((state / "tmp").iterdir()) == []
    # Run again with one asset gone: only that one is uploaded, nothing replaced.
    (assets / f"{_sha('hello-1.0-r0')}.apk").unlink()
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 0, proc.stderr
    assert "1 uploaded" in proc.stdout
    uploads = _calls(state, "release", "upload")
    assert len(uploads) == 3
    assert uploads[-1][-1].endswith(f"/{_sha('hello-1.0-r0')}.apk#hello-1.0-r0.apk")
    assert len(_calls(state, "release", "create")) == 1


def test_mirror_upload_with_nothing_missing_writes_nothing(tmp_path):
    script, state, gh = _mirror_tree(tmp_path)
    for stem in FIXTURE_APKS:
        _asset(state, stem)
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 0, proc.stderr
    assert "0 uploaded" in proc.stdout
    assert "holds all 2 locked packages" in proc.stdout
    assert _writes(_calls(state)) == []


@pytest.mark.parametrize("damage", ["missing", "other bytes"])
def test_mirror_upload_uploads_nothing_while_a_file_isnt_the_locked_one(tmp_path, damage):
    """binutils is fine, hello isn't: every missing file is checked before
    any write, so neither is uploaded and the release isn't created."""
    script, state, gh = _mirror_tree(tmp_path, release=False)
    local = tmp_path / "repo/build/deps/alpine-packages/hello-1.0-r0.apk"
    if damage == "missing":
        local.unlink()
    else:
        local.write_bytes(b"other bytes")
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 1
    if damage == "missing":
        assert f"{local} is missing" in proc.stderr
    else:
        assert f"{local} has sha256 {hashlib.sha256(b'other bytes').hexdigest()}" in proc.stderr
    assert "nothing uploaded: 1 file(s) aren't the locked ones: run `make deps-dev` first" in (
        proc.stderr
    )
    assert _writes(_calls(state)) == []
    assert not (state / "assets").exists()


def test_mirror_upload_names_a_failed_upload_and_check_finds_its_remains(tmp_path):
    script, state, gh = _mirror_tree(tmp_path)
    (state / "fail-upload").write_text("")
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 1
    sha = _sha("binutils-2.43.1-r3")
    assert f"uploading binutils-2.43.1-r3.apk as {sha}.apk failed" in proc.stderr
    assert "may now hold a partial asset" in proc.stderr
    uploads = _calls(state, "release", "upload")
    assert len(uploads) == 1  # stopped at the first failure
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 1
    assert f"gh release delete-asset {MIRROR_TAG} {sha}.apk --repo {MIRROR_REPO}" in proc.stderr


@pytest.mark.parametrize("args", [["sync"], ["check", "extra"], []])
def test_mirror_takes_check_or_upload_only(tmp_path, args):
    script, state, gh = _mirror_tree(tmp_path)
    proc = _run_mirror(script, state, gh, *args)
    assert proc.returncode == 2
    assert "usage:" in proc.stderr
    assert _calls(state) == []


def test_mirror_never_runs_the_real_gh(tmp_path):
    """Without LUMEN_GH the tool runs `gh` from PATH, which in these tests
    is the guard: so a test whose stub stopped applying fails instead of
    reaching GitHub."""
    script, state, _ = _mirror_tree(tmp_path)
    proc = _run_mirror(script, state, None, "upload")
    assert proc.returncode == 1
    assert "gh guard: the real gh is off limits in tests" in proc.stderr
    assert _calls(state) == []
    assert _writes(_calls(state)) == []


@pytest.mark.parametrize("damage", ["partial", "other bytes"])
def test_mirror_check_fails_on_a_broken_asset_with_nothing_missing(tmp_path, damage):
    """Nothing missing, one asset broken: still a failure, saying to delete
    it first."""
    script, state, gh = _mirror_tree(tmp_path, fetched=False)
    _asset(state, "binutils-2.43.1-r3")
    if damage == "partial":
        _asset(state, "hello-1.0-r0")
        (state / "partial").write_text(f"{_sha('hello-1.0-r0')}.apk\n")
    else:
        _asset(state, "hello-1.0-r0", b"other bytes")
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 1
    assert "0 locked package(s) not mirrored, 1 broken: delete each broken asset named above" in (
        proc.stderr
    )
    assert "holds all" not in proc.stdout


def test_mirror_upload_fails_when_its_closing_check_does(tmp_path):
    """Every upload left open: the closing check, read twice, fails the run."""
    script, state, gh = _mirror_tree(tmp_path)
    (state / "upload-open").write_text("")
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 1
    assert "2 uploaded" in proc.stdout
    assert "but its state is 'open'" in proc.stderr
    assert "if no upload is running, delete it by hand" in proc.stderr
    assert "reading it again" in proc.stderr
    assert "holds all" not in proc.stdout


@pytest.mark.parametrize("mode", ["check", "upload"])
def test_mirror_stops_when_the_asset_listing_fails(tmp_path, mode):
    script, state, gh = _mirror_tree(tmp_path)
    (state / "api-error").write_text("")
    proc = _run_mirror(script, state, gh, mode)
    assert proc.returncode == 1
    assert (
        f"can't list the assets of release {MIRROR_TAG} of {MIRROR_REPO}: "
        "HTTP 500: Internal Server Error" in proc.stderr
    )
    assert "isn't in release" not in proc.stderr
    assert _writes(_calls(state)) == []


@pytest.mark.parametrize("mode", ["check", "upload"])
def test_mirror_stops_on_a_release_view_answer_it_cant_read(tmp_path, mode):
    script, state, gh = _mirror_tree(tmp_path)
    (state / "view-garbage").write_text("")
    proc = _run_mirror(script, state, gh, mode)
    assert proc.returncode == 1
    assert f"release {MIRROR_TAG} of {MIRROR_REPO}: gh answered 'null null'" in proc.stderr
    assert _writes(_calls(state)) == []


def test_mirror_check_matches_whole_asset_names(tmp_path):
    """An asset whose name only ends with a locked name doesn't count."""
    script, state, gh = _mirror_tree(tmp_path, fetched=False)
    _asset(state, "binutils-2.43.1-r3")
    sha = _sha("hello-1.0-r0")
    (state / "assets" / f"x{sha}.apk").write_bytes(FIXTURE_APKS["hello-1.0-r0"])
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 1
    assert f"hello-1.0-r0.apk (sha256 {sha}) isn't in release" in proc.stderr


def test_mirror_upload_rehashes_the_staged_copy(tmp_path):
    """A cp that alters the staged copy: the re-hash catches it before any upload."""
    script, state, gh = _mirror_tree(tmp_path)
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    real = shutil.which("cp")
    (bin_dir / "cp").write_text(f'#!/bin/sh\n{real} "$@" && printf x >> "$2"\n')
    (bin_dir / "cp").chmod(0o755)
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 1
    assert "changed while uploading" in proc.stderr
    assert _calls(state, "release", "upload") == []


def test_mirror_upload_sends_the_staged_copy_not_the_source(tmp_path):
    """The source changing after staging doesn't change what is uploaded."""
    script, state, gh = _mirror_tree(tmp_path)
    (state / "touch-source").write_text("")
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 0, proc.stderr
    _assert_fetched_assets(state)


@pytest.mark.parametrize("marker", ["view-fail-once", "api-fail-once"])
def test_mirror_retries_a_gh_read_that_failed_once(tmp_path, marker):
    script, state, gh = _mirror_tree(tmp_path, fetched=False)
    for stem in FIXTURE_APKS:
        _asset(state, stem)
    (state / marker).write_text("")
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 0, proc.stderr
    assert "HTTP 500" in proc.stderr or "HTTP 502" in proc.stderr
    assert "trying again in 0s" in proc.stderr
    call = ("release", "view") if marker.startswith("view") else ("api",)
    assert len(_calls(state, *call)) == 2


def test_mirror_upload_reads_a_lagging_listing_again(tmp_path):
    """The first listing after the uploads shows nothing: the closing check
    reads it once more and passes."""
    script, state, gh = _mirror_tree(tmp_path)
    (state / "lag").write_text("")
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 0, proc.stderr
    assert "reading it again in 0s" in proc.stderr
    assert "isn't in release" not in proc.stderr  # the lagging pass is quiet
    assert "holds all 2 locked packages" in proc.stdout
    assert len(_calls(state, "api")) == 3  # before, lagging, again


def test_mirror_upload_looks_up_a_new_release_again_when_it_lags(tmp_path):
    """A release created just now that `release view` doesn't find yet: the
    closing check's second pass looks it up again, and passes."""
    script, state, gh = _mirror_tree(tmp_path, release=False)
    (state / "view-lag").write_text("")
    proc = _run_mirror(script, state, gh, "upload")
    assert proc.returncode == 0, proc.stderr
    assert "2 uploaded" in proc.stdout
    assert "reading it again in 0s" in proc.stderr
    assert "holds all 2 locked packages" in proc.stdout
    assert len(_calls(state, "release", "create")) == 1


def test_mirror_check_downloads_the_first_file_anonymously(tmp_path):
    """After the listing passes, one anonymous HEAD request at the fetch's
    URL for the first locked file (here the stub's assets, a file:// URL)."""
    script, state, gh = _mirror_tree(tmp_path, fetched=False)
    for stem in FIXTURE_APKS:
        _asset(state, stem)
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 0, proc.stderr
    url = f"file://{state}/assets/{_sha('binutils-2.43.1-r3')}.apk"
    [probe] = (state.parent / "guard/args").read_text().splitlines()
    assert probe == f"-q -fsIL --retry 3 --retry-connrefused --max-time 20 -o /dev/null {url}"


def test_mirror_check_probes_the_real_fetch_scripts_durable_mirror(tmp_path):
    """The probe reads DURABLE_MIRROR out of the real, unmodified
    tools/fetch-alpine-packages.sh (every other probe test rewrites that
    line), so a reformatted line fails here rather than only in CI. The
    curl stub answers the real github.com URL 404."""
    script, state, gh = _mirror_tree(tmp_path, fetched=False)
    shutil.copy2(REPO_ROOT / "tools/fetch-alpine-packages.sh", script.parent)
    for stem in FIXTURE_APKS:
        _asset(state, stem)
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 1
    url = (
        f"https://github.com/{MIRROR_REPO}/releases/download/{MIRROR_TAG}/"
        f"{_sha('binutils-2.43.1-r3')}.apk"
    )
    assert f"is listed but not downloadable at {url}" in proc.stderr


@pytest.mark.parametrize("sleep", ["2s", "0.5", "-1"])
def test_mirror_refuses_a_retry_sleep_that_isnt_whole_seconds(tmp_path, sleep):
    script, state, gh = _mirror_tree(tmp_path)
    proc = _run_mirror(script, state, gh, "check", env={"LUMEN_GH_RETRY_SLEEP": sleep})
    assert proc.returncode == 2
    assert "LUMEN_GH_RETRY_SLEEP" in proc.stderr
    assert _calls(state) == []


def test_mirror_check_fails_a_release_listed_but_not_downloadable(tmp_path):
    script, state, gh = _mirror_tree(tmp_path, fetched=False)
    for stem in FIXTURE_APKS:
        _asset(state, stem)
    _set_durable_mirror(script.parent, f"file://{tmp_path}/nowhere/{{sha256}}.apk")
    proc = _run_mirror(script, state, gh, "check")
    assert proc.returncode == 1
    assert "holds all 2 locked packages" in proc.stdout
    url = f"file://{tmp_path}/nowhere/{_sha('binutils-2.43.1-r3')}.apk"
    assert f"release {MIRROR_TAG} is listed but not downloadable at {url}" in proc.stderr


# --- tools/mkrootfs-dev.sh on fixtures ---------------------------------------------------


def _e2fsprogs() -> tuple[Path, Path]:
    """The real (e2fsck, debugfs), as test_rootfs finds them: skipped
    locally without them, failed in CI (which installs them)."""
    _, e2fsck, debugfs = _rootfs_e2fsprogs()
    return e2fsck, debugfs


def _mkrootfs_dev_tree(tmp_path: Path) -> Path:
    """A stub repository for mkrootfs-dev.sh: its tools, a stub minirootfs,
    the fixture packages and their lock (no build/linux)."""
    repo = tmp_path / "repo"
    (repo / "tools").mkdir(parents=True)
    for name in ("mkrootfs-dev.sh", "alpine-lock.py", "ext4-signed-hash.sh"):
        shutil.copy2(REPO_ROOT / "tools" / name, repo / "tools" / name)
    _stub_minirootfs(repo / "build/deps/alpine-minirootfs")
    lock = _fixture_lock(repo / "build/deps/alpine-packages")
    (repo / "tools/alpine-packages.lock").write_text(lock)
    return repo


def _mkrootfs_dev(repo: Path, out: Path, tmp_path: Path) -> subprocess.CompletedProcess[str]:
    env = {**os.environ, "LUMEN_PYTHON": sys.executable, "TMPDIR": str(tmp_path)}
    return subprocess.run(
        ["bash", str(repo / "tools/mkrootfs-dev.sh"), str(out)],
        capture_output=True,
        text=True,
        check=False,
        env=env,
    )


def test_mkrootfs_dev_builds_the_same_image_twice_from_fixtures(tmp_path):
    """mkrootfs-dev.sh end to end on a stub minirootfs and the fixture
    packages: two builds give the same bytes, e2fsck -fn passes (the
    script's own last step, and again here), the packages' files are in,
    the applet link binutils replaces is a file, and every inode is 0:0."""
    e2fsck, debugfs = _e2fsprogs()
    repo = _mkrootfs_dev_tree(tmp_path)
    first, second = tmp_path / "first.ext4", tmp_path / "second.ext4"
    proc = _mkrootfs_dev(repo, first, tmp_path)
    assert proc.returncode == 0, proc.stderr
    assert "install script" not in proc.stdout
    assert "mkrootfs-dev: busybox applet links replaced: usr/bin/strings" in proc.stdout
    assert re.search(r"mkrootfs-dev: content \d+ MiB in \d+ inodes; image 6\d MiB", proc.stdout)
    assert _mkrootfs_dev(repo, second, tmp_path).returncode == 0
    assert first.read_bytes() == second.read_bytes()
    rootfs.e2fsck_check(first, e2fsck=e2fsck)
    assert {"hello", "hello-again", "strings"} <= rootfs.debugfs_names(
        first, "/usr/bin", tool=debugfs
    )
    assert "Type: regular" in rootfs.debugfs(first, "stat /usr/bin/strings", tool=debugfs)
    stat = rootfs.debugfs(first, "stat /usr/bin/hello", tool=debugfs)
    assert "User:     0   Group:     0" in stat
    assert "Links: 2" in stat
    assert "Mode:  01777" in rootfs.debugfs(first, "stat /tmp", tool=debugfs)
    assert ".version" not in rootfs.debugfs_names(first, "/", tool=debugfs)
    # The feature mask the kernel's ext4 takes (#248) and signed hashes (#308).
    stats = rootfs.debugfs(first, "stats", tool=debugfs)
    features = re.search(r"^Filesystem features:\s+(.*?)\s*$", stats, re.MULTILINE)[1]
    assert features.split() == [
        "ext_attr",
        "resize_inode",
        "dir_index",
        "filetype",
        "extent",
        "64bit",
        "flex_bg",
        "sparse_super",
        "large_file",
        "huge_file",
        "dir_nlink",
        "extra_isize",
    ]
    assert re.search(r"^Filesystem flags:\s+signed_directory_hash\s*$", stats, re.MULTILINE)
    # The label, the fixed UUID ("M8"), no reserved blocks, and /proc's mode,
    # set after the packages.
    assert re.search(r"^Filesystem volume name:\s+lumen-dev$", stats, re.MULTILINE)
    assert re.search(
        r"^Filesystem UUID:\s+4c756d65-6e2d-4d38-8000-000000000001$", stats, re.MULTILINE
    )
    assert re.search(r"^Reserved block count:\s+0$", stats, re.MULTILINE)
    assert "Mode:  0555" in rootfs.debugfs(first, "stat /proc", tool=debugfs)
    # The size and inode count alpine-lock.py's `size` worked out, given to
    # mke2fs (which rounds inodes up to whole groups).
    m = re.search(r"image (\d+) MiB with (\d+) inodes", proc.stdout)
    size_mib, inodes = int(m[1]), int(m[2])
    assert first.stat().st_size == size_mib * 2**20
    inode_count = int(re.search(r"^Inode count:\s+(\d+)", stats, re.MULTILINE)[1])
    per_group = int(re.search(r"^Inodes per group:\s+(\d+)", stats, re.MULTILINE)[1])
    # One block group at this size: mke2fs rounds the count up to a whole
    # inode-table block (16 inodes of 256 bytes).
    assert per_group == inode_count == -(-inodes // 16) * 16
    # Every allocated inode's times are the fixed epoch (not the build's,
    # which two quick builds could share).
    used = inode_count - int(re.search(r"^Free inodes:\s+(\d+)", stats, re.MULTILINE)[1])
    numbers = [2, *range(11, 11 + used + 16)]
    out = subprocess.run(
        [str(debugfs), "-f", "-", str(first)],
        input="".join(f"stat <{n}>\n" for n in numbers),
        capture_output=True,
        text=True,
        check=True,
        env={**os.environ, "LC_ALL": "C"},
    ).stdout
    allocated = 0
    for block in out.split("debugfs: stat")[1:]:
        if re.search(r"^Links: 0\b", block, re.MULTILINE):
            continue
        allocated += 1
        assert re.search(r"^User:\s+0\s+Group:\s+0\b", block, re.MULTILINE), block
        times = re.findall(r"^\s*(c|a|m|cr)time: (0x[0-9a-f]+)", block, re.MULTILINE)
        assert {t for _, t in times} == {"0x6553f100"}, block
    # `used` counts the reserved inodes 1-10 too; only the root among them
    # is stat'ed.
    assert allocated == used - 9
    # No stage, debugfs directory or temporary image left behind.
    assert not [p for p in tmp_path.iterdir() if p.name.startswith("lumen-rootfs-dev")]
    assert not [p for p in tmp_path.iterdir() if ".tmp." in p.name]


def test_mkrootfs_dev_cleans_up_a_read_only_stage(tmp_path):
    _e2fsprogs()
    repo = _mkrootfs_dev_tree(tmp_path)
    lock = _fixture_lock(
        repo / "build/deps/alpine-packages", apks={**FIXTURE_APKS, "ro-1.0-r0": _apk(DATA[6:])}
    )
    (repo / "tools/alpine-packages.lock").write_text(lock)
    proc = _mkrootfs_dev(repo, tmp_path / "o.ext4", tmp_path)
    assert proc.returncode == 0, proc.stderr
    assert not [p for p in tmp_path.iterdir() if p.name.startswith("lumen-rootfs-dev")]


def test_mkrootfs_dev_refuses_a_tampered_package(tmp_path):
    _e2fsprogs()
    repo = _mkrootfs_dev_tree(tmp_path)
    (repo / "build/deps/alpine-packages/hello-1.0-r0.apk").write_bytes(b"tampered")
    out = tmp_path / "out.ext4"
    proc = _mkrootfs_dev(repo, out, tmp_path)
    assert proc.returncode == 1
    assert "sha256" in proc.stderr and "run `make deps-dev`" in proc.stderr
    assert not out.exists()


def test_mkrootfs_dev_refuses_a_package_with_an_unhandled_install_script(tmp_path):
    _e2fsprogs()
    repo = _mkrootfs_dev_tree(tmp_path)
    lock = _fixture_lock(repo / "build/deps/alpine-packages", apks={**FIXTURE_APKS, **SCRIPTED_APK})
    (repo / "tools/alpine-packages.lock").write_text(lock)
    out = tmp_path / "out.ext4"
    proc = _mkrootfs_dev(repo, out, tmp_path)
    assert proc.returncode == 1
    assert "scripted carries .post-install" in proc.stderr
    assert not out.exists()


@pytest.mark.parametrize(
    "sizes",
    [
        "size_mib=70\ninodes=4200\ncontent=1",  # one left out
        "size_mib=70\ninodes=4200\ncontent=1\ninodes_used=x",  # not a number
        "",
    ],
)
def test_mkrootfs_dev_refuses_a_size_it_cant_read(tmp_path, sizes):
    """mkrootfs-dev.sh reads `size`'s key=value lines and stops on a
    missing or non-numeric one instead of passing it to mke2fs."""
    _e2fsprogs()
    repo = _mkrootfs_dev_tree(tmp_path)
    lock_py = repo / "tools/alpine-lock.py"
    lock_py.write_text(
        lock_py.read_text().replace(
            "def cmd_size(a: argparse.Namespace) -> int:\n",
            f"def cmd_size(a: argparse.Namespace) -> int:\n    print({sizes!r})\n    return 0\n",
        )
    )
    out = tmp_path / "out.ext4"
    proc = _mkrootfs_dev(repo, out, tmp_path)
    assert proc.returncode == 1
    assert "mkrootfs-dev: alpine-lock.py size" in proc.stderr
    assert not out.exists()


# --- rootfs.copy_dev_rootfs -----------------------------------------------------------------


def test_copy_dev_rootfs_makes_an_isolated_writable_copy(tmp_path):
    master = tmp_path / "master.ext4"
    master.write_bytes(b"ext4 bytes" * 1000)
    dest = rootfs.copy_dev_rootfs(tmp_path / "test-a", src=master)
    assert dest == tmp_path / "test-a" / "alpine-dev.ext4"
    assert dest.read_bytes() == master.read_bytes()
    with open(dest, "r+b") as f:
        f.write(b"guest wrote")
    assert master.read_bytes() == b"ext4 bytes" * 1000
    # Again over an existing copy: a fresh one.
    assert rootfs.copy_dev_rootfs(tmp_path / "test-a", src=master).read_bytes() == (
        master.read_bytes()
    )


@pytest.mark.parametrize(("platform", "flag"), [("darwin", "-c"), ("linux", "--reflink=auto")])
def test_copy_dev_rootfs_clones_with_the_platforms_cp(tmp_path, monkeypatch, platform, flag):
    master = tmp_path / "master.ext4"
    master.write_bytes(b"m")
    calls = []

    def fake_run(cmd, **kwargs):
        calls.append(cmd)
        Path(cmd[-1]).write_bytes(b"cloned")
        return subprocess.CompletedProcess(cmd, 0)

    monkeypatch.setattr(rootfs.sys, "platform", platform)
    monkeypatch.setattr(rootfs.subprocess, "run", fake_run)
    dest = rootfs.copy_dev_rootfs(tmp_path / "t", src=master)
    assert len(calls) == 1 and calls[0][:3] == ["cp", flag, str(master)]
    assert Path(calls[0][3]).parent == dest.parent
    assert dest.read_bytes() == b"cloned"
    assert sorted(p.name for p in dest.parent.iterdir()) == ["alpine-dev.ext4"]


@pytest.mark.parametrize("failure", ["exit", "missing"])
def test_copy_dev_rootfs_falls_back_to_a_plain_copy(tmp_path, monkeypatch, failure):
    master = tmp_path / "master.ext4"
    master.write_bytes(b"plain")

    def fake_run(cmd, **kwargs):
        if failure == "missing":
            raise FileNotFoundError(cmd[0])
        Path(cmd[-1]).write_bytes(b"partial")
        return subprocess.CompletedProcess(cmd, 1)

    monkeypatch.setattr(rootfs.subprocess, "run", fake_run)
    assert rootfs.copy_dev_rootfs(tmp_path / "t", src=master).read_bytes() == b"plain"


def test_copy_dev_rootfs_refusals(tmp_path):
    master = tmp_path / "master.ext4"
    master.write_bytes(b"m")
    for name in ("", ".", "..", "a/b", "a\\b"):
        with pytest.raises(ValueError, match="invalid rootfs filename"):
            rootfs.copy_dev_rootfs(tmp_path / "t", src=master, name=name)
    with pytest.raises(FileNotFoundError, match="make rootfs-dev"):
        rootfs.copy_dev_rootfs(tmp_path / "t", src=tmp_path / "missing.ext4")
    # The destination is the master: refused, the master kept.
    with pytest.raises(ValueError, match="is the master image itself"):
        rootfs.copy_dev_rootfs(tmp_path, src=master, name="master.ext4")
    assert master.read_bytes() == b"m"
    link_dir = tmp_path / "links"
    link_dir.mkdir()
    (link_dir / "alpine-dev.ext4").symlink_to(master)
    with pytest.raises(ValueError, match="is the master image itself"):
        rootfs.copy_dev_rootfs(link_dir, src=master)
    assert master.read_bytes() == b"m"


def test_copy_dev_rootfs_makes_a_writable_copy_of_a_read_only_master(tmp_path):
    master = tmp_path / "master.ext4"
    master.write_bytes(b"m")
    master.chmod(0o444)
    dest = rootfs.copy_dev_rootfs(tmp_path / "t", src=master)
    assert os.access(dest, os.W_OK)
    assert master.stat().st_mode & 0o777 == 0o444


def test_copy_dev_rootfs_leaves_no_partial_file_when_the_copy_fails(tmp_path, monkeypatch):
    master = tmp_path / "master.ext4"
    master.write_bytes(b"m")
    dest_dir = tmp_path / "t"
    dest_dir.mkdir()
    (dest_dir / "alpine-dev.ext4").write_bytes(b"older copy")

    def fake_run(cmd, **kwargs):
        Path(cmd[-1]).write_bytes(b"partial")
        return subprocess.CompletedProcess(cmd, 1)

    def failing_copy(src, dst):
        raise OSError("disk full")

    monkeypatch.setattr(rootfs.subprocess, "run", fake_run)
    monkeypatch.setattr(rootfs.shutil, "copyfile", failing_copy)
    with pytest.raises(OSError, match="disk full"):
        rootfs.copy_dev_rootfs(dest_dir, src=master)
    assert sorted(p.name for p in dest_dir.iterdir()) == ["alpine-dev.ext4"]
    assert (dest_dir / "alpine-dev.ext4").read_bytes() == b"older copy"


# --- Makefile -------------------------------------------------------------------------------------


def test_make_deps_build_and_test_leave_the_dev_image_out():
    """deps-dev and rootfs-dev are their own targets; `make deps`, `build`
    and `test` don't build the developer image (CI runs its targets as steps
    of their own)."""
    for target in ("deps", "build", "test"):
        # -B: every recipe printed, whatever is up to date.
        dry = subprocess.run(
            ["make", "-n", "-B", "-C", str(REPO_ROOT), target],
            capture_output=True,
            text=True,
            check=False,
        )
        assert dry.returncode == 0, (target, dry.stderr)
        assert "fetch-alpine-packages.sh" not in dry.stdout, target
        assert "mkrootfs-dev.sh" not in dry.stdout, target
    dry = subprocess.run(
        ["make", "-n", "-B", "-C", str(REPO_ROOT), "rootfs-dev"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert dry.returncode == 0, dry.stderr
    assert "tools/fetch-alpine-packages.sh" in dry.stdout
    assert "tools/mkrootfs-dev.sh build/alpine-dev.ext4" in dry.stdout


def test_make_deps_dev_fetches_even_with_its_stamp_up_to_date(tmp_path):
    """deps-dev runs the fetch whatever the stamp says, so it repairs a
    deleted or corrupt .apk (`stage`'s advice); the stamp is fresh here, in
    a BUILD of its own."""
    stamp = tmp_path / "deps/alpine-packages/.lock"
    stamp.parent.mkdir(parents=True)
    shutil.copy(LOCK, stamp)
    os.utime(stamp, (LOCK.stat().st_mtime + 60,) * 2)
    dry = subprocess.run(
        ["make", "-n", "-C", str(REPO_ROOT), "deps-dev", f"BUILD={tmp_path}"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert dry.returncode == 0, dry.stderr
    assert "tools/fetch-alpine-packages.sh" in dry.stdout


# The Makefile's M8_LINUX: M8's test programs, prerequisites of both images.
M8_PROGRAMS = ("m8_proc", "m8_io", "m8_vm", "ptyrun")


@pytest.mark.parametrize("newer", ["tools/alpine-packages.lock", "tools/alpine-lock.py"])
def test_make_rootfs_dev_rebuilds_after_a_lock_or_stager_change(tmp_path, newer):
    files = [
        "deps/alpine-minirootfs/.version",
        "linux/libm7.so",
        "linux/hello",
        "linux/m7_test",
        "linux/m7_vfs",
        *(f"linux/{n}" for n in M8_PROGRAMS),
        "deps/alpine-packages/.lock",
        "alpine-dev.ext4",
    ]
    now = (
        max(
            (REPO_ROOT / t).stat().st_mtime
            for t in (
                "Makefile",
                "tools/mkrootfs-dev.sh",
                "tools/alpine-lock.py",
                "tools/ext4-signed-hash.sh",
                "tools/alpine-packages.lock",
                "tools/fetch-alpine-packages.sh",
                *(
                    f"tools/linux-m8/{n}.rs"
                    for n in (
                        "common",
                        "signals",
                        "exec",
                        "futex",
                        "small",
                        "signal_waits",
                        "threads",
                        "sessions",
                        "timers",
                        *M8_PROGRAMS,
                    )
                ),
            )
        )
        + 200
    )
    for i, f in enumerate(files):
        p = tmp_path / f
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(b"")
        os.utime(p, (now - 100 + i,) * 2)
    run = lambda *extra: (
        subprocess.run(
            ["make", "-n", "-C", str(REPO_ROOT), *extra, "rootfs-dev", f"BUILD={tmp_path}"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )
    assert "mkrootfs-dev.sh" not in run()
    assert "mkrootfs-dev.sh" in run("-W", newer)


# --- The built image (skipped locally without build/alpine-dev.ext4, failed in CI) -----------


@pytest.fixture
def _dev_image():
    _built_or_skip(DEV_IMAGE, DEV_IMAGE.is_file(), "make rootfs-dev")


needs_image = pytest.mark.usefixtures("_dev_image")


@needs_image
def test_dev_image_passes_e2fsck():
    e2fsck, _ = _e2fsprogs()
    rootfs.e2fsck_check(DEV_IMAGE, e2fsck=e2fsck)


@needs_image
def test_dev_image_has_the_gate_tools():
    _, debugfs = _e2fsprogs()
    assert set(GATE_TOOLS) <= rootfs.debugfs_names(DEV_IMAGE, "/usr/bin", tool=debugfs)


@needs_image
def test_dev_image_gate_tools_are_dynamic_elfs_resolving_inside_it(tmp_path):
    """Each gate tool's PT_INTERP and DT_NEEDED resolve inside the image:
    /lib, /usr/lib and /usr/bin dumped out of it with debugfs."""
    _, debugfs = _e2fsprogs()
    root = tmp_path / "root"
    (root / "usr").mkdir(parents=True)
    usr = root / "usr"
    for request in (
        f"rdump /lib {root}",
        f"rdump /usr/lib /usr/bin /usr/libexec /usr/include {usr}",
    ):
        proc = subprocess.run(
            [str(debugfs), "-R", request, str(DEV_IMAGE)],
            capture_output=True,
            text=True,
            check=False,
            env={**os.environ, "LC_ALL": "C"},
        )
        # rdump restores owners, which only root may: those lines aside,
        # any stderr line but the banner is an error.
        errors = [
            line
            for line in proc.stderr.splitlines()
            if line
            and not line.startswith("debugfs ")
            and "Operation not permitted while changing ownership" not in line
        ]
        assert proc.returncode == 0 and not errors, proc.stderr
    for tool in GATE_TOOLS:
        interp, needed = rootfs.validate_dynamic_elf(root / "usr/bin" / tool, root)
        assert interp == rootfs.DEFAULT_INTERP
        assert "libc.musl-x86_64.so.1" in needed, tool
    # What the drivers run: cc1 and cc1plus (isl, mpc, mpfr, gmp, zlib,
    # zstd behind them), collect2, the assembler and the linker.
    (libexec,) = (usr / "libexec/gcc/x86_64-alpine-linux-musl").iterdir()
    for prog in (
        libexec / "cc1",
        libexec / "cc1plus",
        libexec / "collect2",
        usr / "bin/as",
        usr / "bin/ld",
    ):
        interp, needed = rootfs.validate_dynamic_elf(prog, root)
        assert interp == rootfs.DEFAULT_INTERP, prog
    _, needed = rootfs.validate_dynamic_elf(libexec / "cc1plus", root)
    assert {"libisl.so.23", "libmpc.so.3", "libmpfr.so.6", "libgmp.so.10"} <= set(needed)
    # What a compile and link need besides programs: musl-dev's crt files
    # and headers, libstdc++'s headers and library, libgcc.
    for path in (
        "usr/lib/crt1.o",
        "usr/lib/crti.o",
        "usr/lib/libc.so",
        "usr/include/stdio.h",
        f"usr/include/c++/{libexec.name}/iostream",
        "usr/lib/libstdc++.so",
        f"usr/lib/gcc/x86_64-alpine-linux-musl/{libexec.name}/crtbegin.o",
        f"usr/lib/gcc/x86_64-alpine-linux-musl/{libexec.name}/libgcc.a",
    ):
        assert (root / path).exists(), path


@needs_image
def test_dev_image_builds_the_same_twice(tmp_path):
    """Two fresh builds give the same bytes, and the same as
    build/alpine-dev.ext4: the image the other tests read is what the
    current scripts and lock build, not a stale one."""
    first, second = tmp_path / "first.ext4", tmp_path / "second.ext4"
    for out in (first, second):
        proc = subprocess.run(
            ["bash", str(REPO_ROOT / "tools/mkrootfs-dev.sh"), str(out)],
            capture_output=True,
            text=True,
            check=False,
        )
        assert proc.returncode == 0, proc.stderr
    assert al.sha256_file(first) == al.sha256_file(second)
    assert al.sha256_file(first) == al.sha256_file(DEV_IMAGE), "rebuild it: `make rootfs-dev`"
