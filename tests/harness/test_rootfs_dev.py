"""Self-tests for the Alpine developer image (#344): tools/alpine-lock.py's
resolver, lockfile and .apk extraction, tools/fetch-alpine-packages.sh,
tools/mkrootfs-dev.sh and `rootfs.copy_dev_rootfs`.

Everything but the last group runs on fixtures (made-up APKINDEX stanzas,
.apk files built here, `file://` mirrors), with no network. The last group
checks the built image, `build/alpine-dev.ext4` (`make rootfs-dev`), and is
skipped without it, as it is in CI until the gate issues build it there.
"""

from __future__ import annotations

import gzip
import hashlib
import importlib.util
import io
import os
import re
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

import pytest
from lumen_harness import BUILD_DIR, REPO_ROOT, rootfs


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


def _apk(data_entries, *, scripts=None, signed=True, data_eof=True) -> bytes:
    """An .apk: a signature segment (when `signed`), a control segment
    (.PKGINFO and `scripts`, name to body) and a data segment, each its own
    gzip member."""
    control = [(".PKGINFO", "f", b"pkgname = fixture\n", 0o644)]
    control += [(name, "f", body, 0o755) for name, body in (scripts or {}).items()]
    members = []
    if signed:
        members.append(_tar([(".SIGN.RSA.fixture.rsa.pub", "f", b"sig", 0o644)], eof=False))
    members.append(_tar(control, eof=False))
    members.append(_tar(data_entries, eof=data_eof))
    return b"".join(gzip.compress(m, mtime=0) for m in members)


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
        ("1.0_p1-r0", "1.0-r0", 1),
        ("1.0a-r0", "1.0-r0", 1),
        ("6.5_p20241006-r3", "6.5-r3", 1),
        ("23.3.0", "23.2.0", 1),
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
    assert _resolve(["musl", "extradep"]).packages[0].name == "extradep"
    assert len(_resolve(["musl", "extradep"]).packages) == 1


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
    apk = _apk(DATA, scripts={".post-install": b"#!/bin/sh\ntouch /ran\n"})
    written = al.extract_data(apk, tmp_path)
    assert written == [
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
    assert al.install_scripts(apk) == [".post-install"]
    (tmp_path / "usr/share/locked").chmod(0o755)


def test_extract_takes_an_unsigned_apk_and_one_without_end_blocks(tmp_path):
    apk = _apk([("a", "f", b"1", 0o644)], signed=False, data_eof=False)
    assert al.extract_data(apk, tmp_path) == ["a"]
    assert al.install_scripts(apk) == []


def test_extract_never_overwrites_unless_replaceable(tmp_path):
    apk = _apk([("usr/bin/strings", "f", b"binutils", 0o755)])
    (tmp_path / "usr/bin").mkdir(parents=True)
    (tmp_path / "usr/bin/strings").symlink_to("/bin/busybox")
    with pytest.raises(al.LockError, match="usr/bin/strings is already in the image"):
        al.extract_data(apk, tmp_path)
    replaceable = {"usr/bin/strings", "usr/bin/other"}
    al.extract_data(apk, tmp_path, replaceable)
    assert replaceable == {"usr/bin/other"}
    assert (tmp_path / "usr/bin/strings").read_bytes() == b"binutils"
    # A second package with the same file is refused (no longer replaceable).
    with pytest.raises(al.LockError, match="already in the image"):
        al.extract_data(apk, tmp_path, replaceable)


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
        al.extract_data(_apk(entries), tmp_path)


def test_extract_refuses_to_write_through_a_symlink(tmp_path):
    outside = tmp_path / "outside"
    outside.mkdir()
    root = tmp_path / "root"
    (root / "usr").mkdir(parents=True)
    (root / "usr/lib").symlink_to(outside)
    with pytest.raises(al.LockError, match="usr/lib is not a directory"):
        al.extract_data(_apk([("usr/lib/x", "f", b"x", 0o644)]), root)
    assert not list(outside.iterdir())


def test_data_segment_wants_two_or_three_members_in_their_roles():
    one = gzip.compress(_tar([("a", "f", b"1", 0o644)], eof=True))
    with pytest.raises(al.LockError, match="2 or 3 gzip members, this one 1"):
        al.data_segment(one)
    no_pkginfo = one + one
    with pytest.raises(al.LockError, match="no .PKGINFO"):
        al.data_segment(no_pkginfo)
    apk = _apk(DATA)
    bad_sig = (
        gzip.compress(_tar([("x", "f", b"1", 0o644)], eof=False))
        + apk[len(al.gzip_members(apk)[0]) :]
    )
    with pytest.raises(al.LockError, match="first segment isn't a signature"):
        al.data_segment(bad_sig)


# --- busybox's applet links and staging --------------------------------------------

INSTALLED_DB = """\
P:busybox
V:1.37.0-r14
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
    "hello-1.0-r0": _apk(
        DATA[:6], scripts={".post-install": b"#!/bin/sh\ntouch /etc/post-install-ran\n"}
    ),
}


def _fixture_lock(apk_dir: Path, minirootfs: str = "3.21.10") -> str:
    apk_dir.mkdir(parents=True, exist_ok=True)
    lines = [f"# minirootfs {minirootfs}"]
    for stem, data in sorted(FIXTURE_APKS.items()):
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
    assert staged.scripted == [("hello", [".post-install"])]
    assert staged.replaced == ["usr/bin/strings"]
    assert (dest / "usr/bin/strings").read_bytes() == b"binutils strings"
    assert (dest / "usr/bin/hello").read_bytes() == HELLO
    assert not (dest / "etc/post-install-ran").exists()


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
    src = tmp_path / "src"
    (tools / "alpine-packages.lock").write_text(_fixture_lock(src))
    cdn = tmp_path / "cdn/v3.21/main/x86_64"
    cdn.mkdir(parents=True)
    for apk in src.iterdir():
        shutil.copy(apk, cdn)
    flat = tmp_path / "flat"
    flat.mkdir()
    return tools / "fetch-alpine-packages.sh", tmp_path / "repo/build/deps/alpine-packages", flat


def _run_fetch(script: Path, tmp_path: Path, mirror: str = ""):
    env = {
        **os.environ,
        "LUMEN_APK_CDN": f"file://{tmp_path}/cdn",
        "LUMEN_APK_MIRROR": mirror,
    }
    return subprocess.run(
        ["bash", str(script)], capture_output=True, text=True, check=False, env=env
    )


def test_fetch_tries_mirrors_in_order_and_checks_each_sha256(tmp_path):
    script, out, flat = _fetch_tree(tmp_path)
    # The flat mirror has hello right and binutils wrong: hello comes from
    # it, binutils from the CDN after the mismatch.
    (flat / "hello-1.0-r0.apk").write_bytes(FIXTURE_APKS["hello-1.0-r0"])
    (flat / "binutils-2.43.1-r3.apk").write_bytes(b"not binutils")
    shutil.rmtree(tmp_path / "cdn/v3.21/main/x86_64")
    (tmp_path / "cdn/v3.21/main/x86_64").mkdir()
    (tmp_path / "cdn/v3.21/main/x86_64/binutils-2.43.1-r3.apk").write_bytes(
        FIXTURE_APKS["binutils-2.43.1-r3"]
    )
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{file}}")
    assert proc.returncode == 0, proc.stderr
    assert "binutils-2.43.1-r3.apk: sha256" in proc.stderr
    assert "2 packages (2 fetched, 0 already there, 0 stale removed)" in proc.stdout
    for stem, data in FIXTURE_APKS.items():
        assert (out / f"{stem}.apk").read_bytes() == data


def test_fetch_keeps_verified_files_refetches_bad_ones_and_prunes_stale_ones(tmp_path):
    script, out, _ = _fetch_tree(tmp_path)
    assert _run_fetch(script, tmp_path).returncode == 0
    (out / "hello-1.0-r0.apk").write_bytes(b"corrupt")
    (out / "old-0.1-r0.apk").write_bytes(b"stale")
    (out / "notes.txt").write_text("not an .apk: kept")
    proc = _run_fetch(script, tmp_path)
    assert proc.returncode == 0, proc.stderr
    assert "2 packages (1 fetched, 1 already there, 1 stale removed)" in proc.stdout
    assert (out / "hello-1.0-r0.apk").read_bytes() == FIXTURE_APKS["hello-1.0-r0"]
    assert sorted(p.name for p in out.iterdir()) == [
        "binutils-2.43.1-r3.apk",
        "hello-1.0-r0.apk",
        "notes.txt",
    ]


def test_fetch_fails_clearly_when_a_pinned_file_is_gone_everywhere(tmp_path):
    script, out, flat = _fetch_tree(tmp_path)
    (tmp_path / "cdn/v3.21/main/x86_64/hello-1.0-r0.apk").unlink()
    proc = _run_fetch(script, tmp_path, f"file://{flat}/{{file}} file://{tmp_path}/nowhere")
    assert proc.returncode == 1
    assert "no mirror has hello-1.0-r0.apk with sha256" in proc.stderr
    assert f"file://{flat}/hello-1.0-r0.apk" in proc.stderr
    assert f"file://{tmp_path}/nowhere/v3.21/main/x86_64/hello-1.0-r0.apk" in proc.stderr
    assert "Re-run tools/alpine-lock.py" in proc.stderr
    assert not (out / "hello-1.0-r0.apk").exists()
    assert not [p for p in out.iterdir() if p.name.startswith(".part")]


def test_fetch_refuses_a_malformed_lock_line(tmp_path):
    script, _, _ = _fetch_tree(tmp_path)
    lock = script.parent / "alpine-packages.lock"
    lock.write_text(lock.read_text() + "evil;rm 1-r0 main " + "a" * 64 + "\n")
    proc = _run_fetch(script, tmp_path)
    assert proc.returncode == 1
    assert "malformed lock line: evil;rm" in proc.stderr


# --- tools/mkrootfs-dev.sh on fixtures ---------------------------------------------------


def _e2fsprogs() -> tuple[Path, Path]:
    """The real (e2fsck, debugfs): skipped locally without them, failed in
    CI (which installs them)."""
    try:
        _, e2fsck = rootfs.find_e2fsprogs()
    except FileNotFoundError:
        if os.environ.get("CI"):
            raise
        pytest.skip("e2fsprogs not installed")
    debugfs = e2fsck.parent / "debugfs"
    if not os.access(debugfs, os.X_OK):
        if os.environ.get("CI"):
            raise FileNotFoundError(f"no debugfs beside {e2fsck}")
        pytest.skip(f"no debugfs beside {e2fsck}")
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
    the applet link binutils replaces is a file, the post-install script
    never ran, and every inode is 0:0."""
    e2fsck, debugfs = _e2fsprogs()
    repo = _mkrootfs_dev_tree(tmp_path)
    first, second = tmp_path / "first.ext4", tmp_path / "second.ext4"
    proc = _mkrootfs_dev(repo, first, tmp_path)
    assert proc.returncode == 0, proc.stderr
    assert "mkrootfs-dev: install scripts not run: hello: .post-install" in proc.stdout
    assert "mkrootfs-dev: busybox applet links replaced: usr/bin/strings" in proc.stdout
    assert re.search(r"mkrootfs-dev: content \d+ MiB in \d+ inodes; image 6\d MiB", proc.stdout)
    assert _mkrootfs_dev(repo, second, tmp_path).returncode == 0
    assert first.read_bytes() == second.read_bytes()
    rootfs.e2fsck_check(first, e2fsck=e2fsck)
    assert {"hello", "hello-again", "strings"} <= rootfs.debugfs_names(
        first, "/usr/bin", tool=debugfs
    )
    assert "Type: regular" in rootfs.debugfs(first, "stat /usr/bin/strings", tool=debugfs)
    assert "post-install-ran" not in rootfs.debugfs_names(first, "/etc", tool=debugfs)
    stat = rootfs.debugfs(first, "stat /usr/bin/hello", tool=debugfs)
    assert "User:     0   Group:     0" in stat
    assert "Links: 2" in stat
    # No stage or temporary image left behind.
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
    assert calls == [["cp", flag, str(master), str(dest)]]
    assert dest.read_bytes() == b"cloned"


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


# --- Makefile -------------------------------------------------------------------------------------


def test_make_deps_build_and_test_leave_the_dev_image_out():
    """deps-dev and rootfs-dev are their own targets; `make deps`, `build`
    and `test` don't build the developer image (the gate issues wire CI)."""
    for target in ("deps", "build", "test"):
        dry = subprocess.run(
            ["make", "-n", "-C", str(REPO_ROOT), target],
            capture_output=True,
            text=True,
            check=False,
        )
        assert "fetch-alpine-packages.sh" not in dry.stdout, target
        assert "mkrootfs-dev.sh" not in dry.stdout, target
    dry = subprocess.run(
        ["make", "-n", "-B", "-C", str(REPO_ROOT), "rootfs-dev"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert "tools/fetch-alpine-packages.sh" in dry.stdout
    assert "tools/mkrootfs-dev.sh build/alpine-dev.ext4" in dry.stdout


# --- The built image (skipped without build/alpine-dev.ext4) ------------------------------------

needs_image = pytest.mark.skipif(
    not DEV_IMAGE.is_file(), reason=f"{DEV_IMAGE} not built (`make rootfs-dev`)"
)


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
    for request in (f"rdump /lib {root}", f"rdump /usr/lib /usr/bin {root / 'usr'}"):
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


@needs_image
def test_dev_image_builds_the_same_twice(tmp_path):
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
