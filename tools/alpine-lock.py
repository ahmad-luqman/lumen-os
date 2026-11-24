#!/usr/bin/env python3
"""The Alpine developer image's package lock (#344), and the helpers that build
the image from it.

    uv run --frozen python tools/alpine-lock.py lock    # by hand, when bumping
    uv run --frozen python tools/alpine-lock.py stage --lock L --apk-dir D --minirootfs M DEST
    uv run --frozen python tools/alpine-lock.py size STAGE
    uv run --frozen python tools/alpine-lock.py files [--lock L]

`lock` reads Alpine v3.21's `APKINDEX.tar.gz` (`main`, then `community` for a
name `main` lacks), resolves the closure of `tools/alpine-packages.txt` through
`D:` dependencies, `p:` provides (`so:` and `cmd:` included) and `i:`
install-if rules, leaving out the minirootfs' packages (its
`lib/apk/db/installed`), downloads each `.apk` once to record its sha256 (an
APKINDEX's `C:` is a SHA-1 of the control segment, not a file hash; it is
checked against the download, which ties the file to the index), and writes
`tools/alpine-packages.lock`. It keeps the downloads in `build/deps/alpine-packages/`,
where `tools/fetch-alpine-packages.sh` would put them.

`stage` and `size` are `tools/mkrootfs-dev.sh`'s: `stage` checks every locked
`.apk`'s sha256, then extracts each one's data segment only (never its install
scripts; a package carrying any is refused unless `HANDLED_SCRIPTS` says how
the image reproduces them) into a staged root; `size` works out the image's
size and inode count from the staged content, the same on every host. `files`
is `tools/fetch-alpine-packages.sh`'s: the lock, parsed and checked here, as
`repo file sha256` lines, so the lock has one reader.

Standard library only, so the harness tests import it and `LUMEN_PYTHON` can
run it without uv (the tests' stub trees).
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import os
import re
import shutil
import stat
import sys
import tarfile
import urllib.request
import zlib
from collections import deque
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ALPINE_BRANCH = "v3.21"
ARCH = "x86_64"
DEFAULT_MIRROR = "https://dl-cdn.alpinelinux.org/alpine"
DEFAULT_REPOS = ("main", "community")
LOCK_HEADER = "# tools/alpine-packages.lock: written by tools/alpine-lock.py; do not edit."
_SHA256 = re.compile(r"[0-9a-f]{64}")
# A package name or version: what apk accepts, and nothing a shell or a URL
# would read differently.
_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9+._-]*")
_VERSION = re.compile(r"[0-9][A-Za-z0-9._~]*-r[0-9]+")
_DEP = re.compile(r"(!?)([^<>=~!]+)(?:(<=|>=|=~|<|>|=|~)(.+))?")


class LockError(Exception):
    """A resolution, lockfile or archive that can't be used, with why."""


# --- Versions (apk-tools' ordering, as far as Alpine's packages use it) ------

_SUFFIX_RANK = {
    "alpha": 0,
    "beta": 1,
    "pre": 2,
    "rc": 3,
    "cvs": 5,
    "svn": 6,
    "git": 7,
    "hg": 8,
    "p": 9,
}
# No suffix sorts between `_rc` and `_cvs`: every key ends with it, so a
# version whose suffixes run out compares as having this one next.
_NO_SUFFIX = (4, 0)
_VERSION_PARTS = re.compile(
    r"(?P<nums>[0-9]+(?:\.[0-9]+)*)(?P<letter>[a-z]?)(?P<sufs>(?:_[a-z]+[0-9]*)*)"
    r"(?:~[0-9a-f]+)?(?:-r(?P<rev>[0-9]+))?"
)


def _version_key(version: str) -> tuple:
    """`version` as a tuple that sorts in apk's order (`vercmp`)."""
    m = _VERSION_PARTS.fullmatch(version)
    if m is None:
        raise LockError(f"can't compare version {version!r}")
    nums = tuple(int(n) for n in m["nums"].split("."))
    sufs = []
    for suf in filter(None, m["sufs"].split("_")):
        word = suf.rstrip("0123456789")
        if word not in _SUFFIX_RANK:
            raise LockError(f"unknown suffix _{word} in version {version!r}")
        sufs.append((_SUFFIX_RANK[word], int(suf[len(word) :] or 0)))
    sufs.append(_NO_SUFFIX)
    return nums, m["letter"], tuple(sufs), int(m["rev"] or 0)


def vercmp(a: str, b: str) -> int:
    """-1, 0 or 1 as `a` sorts before, with or after `b` in apk's order:
    numbers, then a letter, then `_alpha` < `_beta` < `_pre` < `_rc` < none
    < `_cvs` < `_svn` < `_git` < `_hg` < `_p`, then `-rN`. (apk compares a
    later segment with a leading zero as a decimal fraction; no constraint
    in the closure needs that.)"""
    ka, kb = _version_key(a), _version_key(b)
    return (ka > kb) - (ka < kb)


# --- APKINDEX and the installed database --------------------------------------


@dataclass(frozen=True)
class Dep:
    """One `D:` or `i:` atom: a name (maybe `so:` or `cmd:`), an optional
    version constraint, or a conflict (`!name`)."""

    name: str
    op: str | None = None
    version: str | None = None
    conflict: bool = False

    def allows(self, version: str | None) -> bool:
        """Whether a package or provide at `version` meets the constraint (a
        provide without a version meets only an unconstrained atom)."""
        if self.op is None:
            return True
        if version is None:
            return False
        if self.op == "~" or self.op == "=~":
            return version == self.version or version.startswith(
                tuple(self.version + c for c in ".-_")
            )
        c = vercmp(version, self.version)
        return {"=": c == 0, "<": c < 0, ">": c > 0, "<=": c <= 0, ">=": c >= 0}[self.op]

    def __str__(self) -> str:
        return ("!" if self.conflict else "") + self.name + (self.op or "") + (self.version or "")


def parse_dep(atom: str) -> Dep:
    m = _DEP.fullmatch(atom)
    if m is None or not m[2]:
        raise LockError(f"can't parse dependency {atom!r}")
    return Dep(m[2], m[3], m[4], bool(m[1]))


@dataclass
class Package:
    name: str
    version: str
    repo: str
    deps: tuple[Dep, ...] = ()
    provides: tuple[tuple[str, str | None], ...] = ()
    install_if: tuple[Dep, ...] = ()
    priority: int = 0
    checksum: str | None = None
    size: int | None = None

    @property
    def filename(self) -> str:
        return f"{self.name}-{self.version}.apk"

    def provided_version(self, name: str) -> str | None:
        """The version this package gives `name`: its own, or a provide's."""
        if name == self.name:
            return self.version
        return next((ver for pname, ver in self.provides if pname == name), None)

    def satisfies(self, dep: Dep) -> bool:
        if dep.name == self.name and dep.allows(self.version):
            return True
        return any(name == dep.name and dep.allows(ver) for name, ver in self.provides)


def parse_index(text: str, repo: str) -> dict[str, Package]:
    """An APKINDEX (or `lib/apk/db/installed`, the same format) by package
    name: blank-line separated stanzas of `X:value` lines; keys it doesn't
    use (file lists, maintainers) are skipped. A name listed at two
    versions (two origins' subpackages) keeps the higher, as apk picks."""
    packages: dict[str, Package] = {}
    for stanza in re.split(r"\n\s*\n", text):
        fields: dict[str, str] = {}
        for line in stanza.splitlines():
            if len(line) >= 2 and line[1] == ":" and line[0] in "PVDpikCS":
                fields.setdefault(line[0], line[2:])
        if "P" not in fields:
            continue
        name, version = fields["P"], fields.get("V", "")
        if not _NAME.fullmatch(name) or not _VERSION.fullmatch(version):
            raise LockError(f"{repo}: bad package name or version {name!r} {version!r}")
        if name in packages and vercmp(packages[name].version, version) >= 0:
            continue
        provides = []
        for atom in fields.get("p", "").split():
            pname, _, pver = atom.partition("=")
            provides.append((pname, pver or None))
        packages[name] = Package(
            name=name,
            version=version,
            repo=repo,
            deps=tuple(parse_dep(a) for a in fields.get("D", "").split()),
            provides=tuple(provides),
            install_if=tuple(parse_dep(a) for a in fields.get("i", "").split()),
            priority=int(fields.get("k", "0")),
            checksum=fields.get("C"),
            size=int(fields["S"]) if "S" in fields else None,
        )
    return packages


def parse_wanted(text: str) -> list[str]:
    """`tools/alpine-packages.txt`: a name per line, `#` comments."""
    names = []
    for n, line in enumerate(text.splitlines(), 1):
        word = line.split("#", 1)[0].strip()
        if not word:
            continue
        if not _NAME.fullmatch(word):
            raise LockError(f"alpine-packages.txt:{n}: not a package name: {word!r}")
        if word in names:
            raise LockError(f"alpine-packages.txt:{n}: {word} listed twice")
        names.append(word)
    if not names:
        raise LockError("alpine-packages.txt names no package")
    return names


# --- Resolution ----------------------------------------------------------------


@dataclass(frozen=True)
class Resolution:
    packages: tuple[Package, ...]

    @property
    def repos(self) -> list[str]:
        """The repositories the packages came from, sorted."""
        return sorted({p.repo for p in self.packages})


def resolve(
    wanted: list[str],
    available: dict[str, Package],
    installed: dict[str, Package],
) -> Resolution:
    """The packages `wanted` needs that `installed` doesn't have, from
    `available`, sorted by name.

    A dependency is met by an installed or already chosen package with that
    name or provide (its version constraint checked); otherwise by the
    available package of that name, or else by a provider of it: the
    highest `k:` priority, then the highest version of what it provides
    (apk's order: isl26's `so:libisl.so.23=23.3.0` over isl25's 23.2.0),
    and a tie an error (name one in alpine-packages.txt to choose). Then,
    as apk does, every package whose `i:` atoms are all met joins, with its
    dependencies, until none does.
    An `i:` atom `!name` holds while nothing meets `name`. A conflict
    (`!name`) in `D:` met by the result is an error, and so is an installed
    package too old or too new for an atom naming it."""
    chosen: dict[str, Package] = {}

    def met(dep: Dep) -> bool:
        return any(p.satisfies(dep) for p in (*installed.values(), *chosen.values()))

    def holds(atom: Dep) -> bool:
        """An `i:` atom: met, or for `!name`, not met."""
        return met(atom) != atom.conflict

    def candidate(dep: Dep, needed_by: str) -> Package:
        pkg = available.get(dep.name)
        if pkg is not None:
            if not dep.allows(pkg.version):
                raise LockError(
                    f"{needed_by} needs {dep}, but the index has {pkg.name}-{pkg.version}"
                )
            return pkg
        providers = [p for p in available.values() if p.satisfies(dep)]
        if not providers:
            raise LockError(f"{needed_by} needs {dep}, which no package provides")

        def rank(p: Package) -> tuple:
            v = p.provided_version(dep.name)
            return p.priority, v is not None, _version_key(v) if v is not None else ()

        providers.sort(key=rank, reverse=True)
        if len(providers) > 1 and rank(providers[0]) == rank(providers[1]):
            names = ", ".join(sorted(p.name for p in providers))
            raise LockError(
                f"{needed_by} needs {dep}, which {names} all provide: "
                "name the one to use in tools/alpine-packages.txt"
            )
        return providers[0]

    queue: deque[tuple[Dep, str]] = deque()

    def choose(pkg: Package) -> None:
        chosen[pkg.name] = pkg
        queue.extend((d, pkg.name) for d in pkg.deps if not d.conflict)

    def drain() -> None:
        while queue:
            dep, needed_by = queue.popleft()
            if met(dep):
                continue
            # Unmet, so an installed package of that name is the wrong version.
            if (wrong := installed.get(dep.name)) is not None:
                raise LockError(
                    f"{needed_by} needs {dep}, but the minirootfs has {wrong.name}-{wrong.version}"
                )
            pkg = candidate(dep, needed_by)
            # An index package chosen already would have met `dep`; one the
            # minirootfs has at another version may provide it here.
            if pkg.name in installed:
                raise LockError(
                    f"{needed_by} needs {dep}, which {pkg.name}-{pkg.version} doesn't meet"
                )
            choose(pkg)

    for name in wanted:
        if name in installed:
            continue
        if name not in available:
            providers = sorted(p.name for p in available.values() if p.satisfies(Dep(name)))
            hint = f" ({', '.join(providers)} provide it: name one)" if providers else ""
            raise LockError(f"alpine-packages.txt names {name}, which the index lacks{hint}")
        choose(available[name])
    drain()

    while True:
        joining = [
            p
            for name, p in sorted(available.items())
            if p.install_if
            and name not in chosen
            and name not in installed
            and all(holds(d) for d in p.install_if)
        ]
        if not joining:
            break
        for pkg in joining:
            choose(pkg)
        drain()

    everything = (*installed.values(), *chosen.values())
    for pkg in chosen.values():
        for dep in pkg.deps:
            if dep.conflict:
                clash = [p.name for p in everything if p.name != pkg.name and p.satisfies(dep)]
                if clash:
                    raise LockError(f"{pkg.name} conflicts with {dep.name} ({', '.join(clash)})")

    return Resolution(tuple(sorted(chosen.values(), key=lambda p: p.name)))


def merge_repos(indexes: list[dict[str, Package]]) -> dict[str, Package]:
    """Several repositories' packages as one: an earlier repository's name
    hides a later one's (`main` before `community`)."""
    merged: dict[str, Package] = {}
    for index in indexes:
        for name, pkg in index.items():
            merged.setdefault(name, pkg)
    return merged


# --- The lockfile --------------------------------------------------------------


@dataclass(frozen=True)
class Locked:
    name: str
    version: str
    repo: str
    sha256: str

    @property
    def filename(self) -> str:
        return f"{self.name}-{self.version}.apk"


@dataclass(frozen=True)
class Lock:
    minirootfs: str
    packages: tuple[Locked, ...]


def parse_lock(text: str) -> Lock:
    """`tools/alpine-packages.lock`: `#` header lines (one `# minirootfs
    <version>`), then `name version repo sha256` lines, sorted by name, each
    name once."""
    minirootfs = None
    packages: list[Locked] = []
    for n, line in enumerate(text.splitlines(), 1):
        if line.startswith("#"):
            words = line[1:].split()
            if words[:1] == ["minirootfs"]:
                if minirootfs is not None or len(words) != 2:
                    raise LockError(f"lock:{n}: a second or malformed minirootfs line")
                minirootfs = words[1]
            continue
        words = line.split()
        if len(words) != 4:
            raise LockError(f"lock:{n}: want `name version repo sha256`, got {line!r}")
        name, version, repo, sha = words
        if not _NAME.fullmatch(name) or not _VERSION.fullmatch(version):
            raise LockError(f"lock:{n}: bad name or version {name!r} {version!r}")
        if repo not in DEFAULT_REPOS:
            raise LockError(f"lock:{n}: unknown repository {repo!r}")
        if not _SHA256.fullmatch(sha):
            raise LockError(f"lock:{n}: bad sha256 {sha!r}")
        if packages and name <= packages[-1].name:
            raise LockError(f"lock:{n}: {name} is out of order or listed twice")
        packages.append(Locked(name, version, repo, sha))
    if minirootfs is None:
        raise LockError("lock: no `# minirootfs <version>` line")
    if not packages:
        raise LockError("lock: no packages")
    return Lock(minirootfs, tuple(packages))


def format_lock(
    resolution: Resolution,
    hashes: dict[str, str],
    minirootfs: str,
    installed: int,
    indexes: dict[str, tuple[str, str]],
) -> str:
    lines = [
        LOCK_HEADER,
        f"# Alpine {ALPINE_BRANCH} {ARCH}, the closure of tools/alpine-packages.txt less the",
        f"# minirootfs' {installed} packages, resolved from:",
    ]
    for repo in resolution.repos:
        description, sha = indexes[repo]
        lines.append(f"#   {repo}/{ARCH}/APKINDEX.tar.gz {description} sha256 {sha}")
    lines += [f"# minirootfs {minirootfs}", "# name version repo sha256"]
    missing = [p.name for p in resolution.packages if p.name not in hashes]
    if missing:
        raise LockError(f"no sha256 for {', '.join(missing)}")
    lines += [f"{p.name} {p.version} {p.repo} {hashes[p.name]}" for p in resolution.packages]
    return "\n".join(lines) + "\n"


# --- .apk files ------------------------------------------------------------------

_CHUNK = 1 << 20


def gzip_members(data: bytes) -> list[bytes]:
    """The gzip members an `.apk` concatenates, each as its compressed bytes:
    a signed package's signature, control and data segments."""
    members = []
    pos = 0
    while pos < len(data):
        if data[pos : pos + 2] != b"\x1f\x8b":
            raise LockError(f"no gzip member at byte {pos}")
        d = zlib.decompressobj(wbits=31)
        tail = memoryview(data)[pos:]
        while not d.eof:
            if not tail:
                raise LockError(f"the gzip member at byte {pos} is truncated")
            d.decompress(tail, _CHUNK)  # the output, bounded, is dropped
            tail = d.unconsumed_tail
        end = len(data) - len(d.unused_data)
        members.append(data[pos:end])
        pos = end
    return members


def control_checksum(data: bytes) -> str:
    """The APKINDEX `C:` value for an `.apk`: `Q1` and the base64 SHA-1 of its
    control segment (the member before the data)."""
    members = gzip_members(data)
    if len(members) < 2:
        raise LockError("an .apk has at least a control and a data segment")
    return "Q1" + base64.b64encode(hashlib.sha1(members[-2]).digest()).decode()


def _member_names(member: bytes) -> list[str]:
    with tarfile.open(fileobj=io.BytesIO(member), mode="r:gz") as tar:
        return tar.getnames()


@dataclass(frozen=True)
class ApkParts:
    """An `.apk` split once: the scripts its control segment carries
    (`.pre-install`, `.post-install`, `.trigger`, ...: what `stage` never
    runs) and its data segment (a gzipped tar)."""

    scripts: tuple[str, ...]
    data: bytes


def split_apk(data: bytes) -> ApkParts:
    """An `.apk`'s parts: the last gzip member is its data segment, after
    the control segment (`.PKGINFO`, the install scripts) and, when signed,
    the signature (`.SIGN.*`). Checked by what each holds."""
    members = gzip_members(data)
    if len(members) not in (2, 3):
        raise LockError(f"an .apk has 2 or 3 gzip members, this one {len(members)}")
    control = _member_names(members[-2])
    if ".PKGINFO" not in control:
        raise LockError("the .apk's control segment has no .PKGINFO")
    if len(members) == 3 and not all(n.startswith(".SIGN.") for n in _member_names(members[0])):
        raise LockError("the .apk's first segment isn't a signature")
    return ApkParts(tuple(sorted(n for n in control if n != ".PKGINFO")), members[-1])


def _safe_parts(name: str) -> list[str]:
    parts = [p for p in name.split("/") if p not in ("", ".")]
    if not parts or name.startswith("/") or ".." in parts:
        raise LockError(f"unsafe path in .apk: {name!r}")
    if len(parts) == 1 and parts[0].startswith((".PKGINFO", ".SIGN.")):
        raise LockError(f"control file {name!r} in a data segment")
    return parts


def _real_parent(dest: Path, parts: list[str]) -> Path:
    """`dest/parts[:-1]`, refusing a symlink on the way (a write through one
    could leave the staged root)."""
    here = dest
    for part in parts[:-1]:
        here = here / part
        if here.is_symlink() or (here.exists() and not here.is_dir()):
            raise LockError(f"{here.relative_to(dest)} is not a directory")
        if not here.exists():
            here.mkdir(mode=0o755)
    return here


@dataclass(frozen=True)
class Extracted:
    """What `extract_data` wrote, in archive order (directories with a
    trailing `/`), which of the `replaceable` paths it replaced, and each
    directory's mode from the archive, for `apply_dir_modes` once every
    package is in."""

    written: tuple[str, ...]
    replaced: frozenset[str]
    dir_modes: dict[Path, int]


def apply_dir_modes(dir_modes: dict[Path, int]) -> None:
    """Directories' modes, deepest first: last, so a read-only directory
    (`usr/share/x` at 0555) never stops a later file going in."""
    for path, mode in sorted(dir_modes.items(), reverse=True):
        os.chmod(path, mode)


def _extracted_file(dest: Path, parts: list[str]) -> Path | None:
    """`dest/parts` if it is a regular file reached through real
    directories only (no symlink on the way, nor at the end: `os.link`
    would follow one, maybe out of the staged root), else None."""
    here = dest
    for part in parts[:-1]:
        here = here / part
        if here.is_symlink() or not here.is_dir():
            return None
    here = here / parts[-1]
    return None if here.is_symlink() or not here.is_file() else here


def extract_data(apk: ApkParts, dest: Path, replaceable: frozenset[str] = frozenset()) -> Extracted:
    """Extract an `.apk`'s data segment into `dest`. Stricter than apk,
    which overwrites any file no package owns: a path already in `dest` (the
    minirootfs', or another package's) is an error, unless both are
    directories or the path is in `replaceable` (the busybox applet links
    `stage` passes, the only unowned files the minirootfs has in the way).
    New directories are made at 0755 and their archive modes handed back
    (`Extracted.dir_modes`), for the caller to apply when every package is
    in (`apply_dir_modes`), so a read-only one can still be filled; owners
    and times are left to mkrootfs-dev.sh."""
    written = []
    replaced = set()
    dir_modes: dict[Path, int] = {}
    with tarfile.open(fileobj=io.BytesIO(apk.data), mode="r:gz") as tar:
        for m in tar:
            parts = _safe_parts(m.name)
            parent = _real_parent(dest, parts)
            path = parent / parts[-1]
            rel = "/".join(parts)
            if m.isdir():
                if path.is_symlink() or (path.exists() and not path.is_dir()):
                    raise LockError(f"{rel}: a directory over a file")
                path.mkdir(mode=0o755, exist_ok=True)
                dir_modes[path] = m.mode & 0o7777
                written.append(rel + "/")
                continue
            if os.path.lexists(path):
                if rel not in replaceable or rel in replaced:
                    raise LockError(f"{rel} is already in the image")
                replaced.add(rel)
                path.unlink()
            if m.isreg():
                src = tar.extractfile(m)
                assert src is not None
                with open(path, "xb") as out:
                    shutil.copyfileobj(src, out, _CHUNK)
                os.chmod(path, m.mode & 0o7777)
            elif m.issym():
                os.symlink(m.linkname, path)
            elif m.islnk():
                target = _extracted_file(dest, _safe_parts(m.linkname))
                if target is None:
                    raise LockError(f"{rel}: hard link to {m.linkname}, not an extracted file")
                os.link(target, path, follow_symlinks=False)
            else:
                raise LockError(f"{rel}: a device node or FIFO, which the image doesn't take")
            written.append(rel)
    return Extracted(tuple(written), frozenset(replaced), dir_modes)


BUSYBOX = "/bin/busybox"
BUSYBOX_TRIGGER_DIRS = ("bin", "usr/bin", "sbin", "usr/sbin")


def owned_paths(installed_db: str) -> set[str]:
    """The paths `lib/apk/db/installed` says its packages own: each `R:`
    file under the `F:` directory before it."""
    owned = set()
    directory = ""
    for line in installed_db.splitlines():
        if line.startswith("F:"):
            directory = line[2:]
        elif line.startswith("R:"):
            owned.add(f"{directory}/{line[2:]}" if directory else line[2:])
        elif not line.strip():
            directory = ""
    return owned


def busybox_links(root: Path, owned: set[str]) -> set[str]:
    """The applet links busybox's trigger made (`busybox --install -s`):
    symlinks to /bin/busybox in the directories it watches that no package
    owns. apk lets a package's file replace one, since no package owns it,
    and the trigger, run again, makes links only where nothing is, so a
    replaced link stays replaced."""
    links = set()
    for d in BUSYBOX_TRIGGER_DIRS:
        here = root / d
        if not here.is_dir() or here.is_symlink():
            continue
        for entry in here.iterdir():
            rel = f"{d}/{entry.name}"
            if entry.is_symlink() and os.readlink(entry) == BUSYBOX and rel not in owned:
                links.add(rel)
    return links


def sha256_file(path: Path) -> str:
    with open(path, "rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


# Install scripts the image reproduces statically: (package, script) to how,
# printed by `stage`; tools/mkrootfs-dev.sh does the reproducing. Empty: no
# locked package carries a script (#344). A lock bump that brings one fails
# `stage` until it is worked out and listed here.
HANDLED_SCRIPTS: dict[tuple[str, str], str] = {}


@dataclass(frozen=True)
class Staged:
    """What `stage` did beyond extracting: the install scripts it let pass
    (`HANDLED_SCRIPTS`' entries, with how each is reproduced) and the
    busybox applet links packages' files replaced."""

    handled: tuple[tuple[str, str, str], ...]
    replaced: tuple[str, ...]


def stage(lock: Lock, apk_dir: Path, minirootfs: Path, dest: Path) -> Staged:
    """Every locked package's data into `dest` (the minirootfs already
    copied there), in lock order, once every `.apk` has matched its sha256
    and none carries a script `HANDLED_SCRIPTS` doesn't list. Each file is
    read once: the bytes hashed are the bytes extracted."""
    version_file = minirootfs / ".version"
    have = version_file.read_text().strip() if version_file.is_file() else None
    if have != lock.minirootfs:
        raise LockError(
            f"the lock was resolved against minirootfs {lock.minirootfs}, "
            f"but {minirootfs} has {have}: re-run tools/alpine-lock.py"
        )
    apks = []
    for pkg in lock.packages:
        path = apk_dir / pkg.filename
        if not path.is_file():
            raise LockError(f"{path} is missing: run `make deps-dev`")
        data = path.read_bytes()
        got = hashlib.sha256(data).hexdigest()
        if got != pkg.sha256:
            raise LockError(
                f"{path}: sha256 {got}, the lock says {pkg.sha256}: run `make deps-dev`"
            )
        apks.append((pkg.name, split_apk(data)))
    handled = []
    for name, apk in apks:
        for script in apk.scripts:
            how = HANDLED_SCRIPTS.get((name, script))
            if how is None:
                raise LockError(
                    f"{name} carries {script}, which the image never runs: work out what it "
                    "does, reproduce it in tools/mkrootfs-dev.sh and list it in "
                    "tools/alpine-lock.py's HANDLED_SCRIPTS"
                )
            handled.append((name, script, how))
    owned = owned_paths((minirootfs / "lib/apk/db/installed").read_text())
    links = frozenset(busybox_links(dest, owned))
    replaced: set[str] = set()
    dir_modes: dict[Path, int] = {}
    for _, apk in apks:
        out = extract_data(apk, dest, links - replaced)
        replaced |= out.replaced
        dir_modes |= out.dir_modes
    apply_dir_modes(dir_modes)
    return Staged(tuple(handled), tuple(sorted(replaced)))


# --- Image size --------------------------------------------------------------------

BLOCK = 4096
MIB = 1 << 20
FREE_HEADROOM = 64 * MIB
INODE_HEADROOM = 4096
INODE_SIZE = 256


@dataclass(frozen=True)
class ImageSize:
    content: int
    inodes_used: int
    size_mib: int
    inodes: int


def image_size(stage_dir: Path) -> ImageSize:
    """The image's size and inode count from the staged tree, counted the
    same on every host: each regular file's length in whole 4 KiB blocks
    (hard links once), a block per directory and per symlink too long to
    sit in its inode (60 bytes or more); then 64 MiB of free space and a
    quarter more inodes (at least 4096) for what the gates write, and room
    for the inode tables."""
    content = 0
    inodes = 1
    seen: set[tuple[int, int]] = set()
    for dirpath, dirnames, filenames in os.walk(stage_dir):
        for name in (*dirnames, *filenames):
            st = os.lstat(os.path.join(dirpath, name))
            if (st.st_dev, st.st_ino) in seen:
                continue
            seen.add((st.st_dev, st.st_ino))
            inodes += 1
            if stat.S_ISREG(st.st_mode):
                content += -(-st.st_size // BLOCK) * BLOCK
            elif stat.S_ISDIR(st.st_mode) or (stat.S_ISLNK(st.st_mode) and st.st_size >= 60):
                content += BLOCK
    content += BLOCK  # the root directory
    total_inodes = inodes + max(inodes // 4, INODE_HEADROOM)
    size = content + FREE_HEADROOM + total_inodes * INODE_SIZE
    return ImageSize(content, inodes, -(-size // MIB), total_inodes)


# --- lock: network and command line --------------------------------------------------


def _fetch(url: str) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": "lumen-alpine-lock"})
    with urllib.request.urlopen(req, timeout=120) as resp:
        return resp.read()


def _read_index(blob: bytes, repo: str) -> tuple[dict[str, Package], str]:
    with tarfile.open(fileobj=io.BytesIO(blob), mode="r:gz") as tar:

        def text(name: str) -> str:
            f = tar.extractfile(name)
            if f is None:
                raise LockError(f"{repo}: APKINDEX.tar.gz has no {name}")
            return f.read().decode()

        return parse_index(text("APKINDEX"), repo), text("DESCRIPTION").strip()


def cmd_lock(a: argparse.Namespace) -> int:
    wanted = parse_wanted(a.packages.read_text())
    version_file = a.minirootfs / ".version"
    db = a.minirootfs / "lib/apk/db/installed"
    if not version_file.is_file() or not db.is_file():
        raise LockError(f"no minirootfs in {a.minirootfs}: run `make deps`")
    installed = parse_index(db.read_text(), "installed")
    indexes: dict[str, tuple[str, str]] = {}
    parsed = []
    for repo in a.repo or DEFAULT_REPOS:
        url = f"{a.mirror}/{ALPINE_BRANCH}/{repo}/{ARCH}/APKINDEX.tar.gz"
        print(f"alpine-lock: reading {url}", file=sys.stderr)
        blob = _fetch(url)
        index, description = _read_index(blob, repo)
        indexes[repo] = (description, hashlib.sha256(blob).hexdigest())
        parsed.append(index)
    resolution = resolve(wanted, merge_repos(parsed), installed)

    a.apk_dir.mkdir(parents=True, exist_ok=True)
    hashes = {}
    for pkg in resolution.packages:
        # The index's size and control checksum are what tie a download to
        # the index: a stanza without them can't be locked.
        if pkg.checksum is None or pkg.size is None:
            raise LockError(f"{pkg.repo}: the index gives {pkg.name} no C: or S:")
        path = a.apk_dir / pkg.filename
        downloaded = not path.is_file()
        if downloaded:
            url = f"{a.mirror}/{ALPINE_BRANCH}/{pkg.repo}/{ARCH}/{pkg.filename}"
            print(f"alpine-lock: downloading {pkg.filename}", file=sys.stderr)
            data = _fetch(url)
        else:
            data = path.read_bytes()
        if len(data) != pkg.size:
            raise LockError(f"{pkg.filename}: {len(data)} bytes, the index says {pkg.size}")
        if control_checksum(data) != pkg.checksum:
            raise LockError(f"{pkg.filename}: its control segment doesn't match the index's C:")
        if downloaded:
            tmp = path.with_suffix(".apk.part")
            tmp.write_bytes(data)
            tmp.replace(path)
        hashes[pkg.name] = hashlib.sha256(data).hexdigest()

    minirootfs = version_file.read_text().strip()
    a.lock.write_text(format_lock(resolution, hashes, minirootfs, len(installed), indexes))
    print(f"alpine-lock: {len(resolution.packages)} packages into {a.lock}", file=sys.stderr)
    return 0


def cmd_stage(a: argparse.Namespace) -> int:
    staged = stage(parse_lock(a.lock.read_text()), a.apk_dir, a.minirootfs, a.dest)
    for name, script, how in staged.handled:
        print(f"install script not run, reproduced: {name} {script}: {how}")
    if staged.replaced:
        print(f"busybox applet links replaced: {' '.join(staged.replaced)}")
    return 0


def cmd_size(a: argparse.Namespace) -> int:
    s = image_size(a.stage)
    for key in ("size_mib", "inodes", "content", "inodes_used"):
        print(f"{key}={getattr(s, key)}")
    return 0


def cmd_files(a: argparse.Namespace) -> int:
    for pkg in parse_lock(a.lock.read_text()).packages:
        print(pkg.repo, pkg.filename, pkg.sha256)
    return 0


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="alpine-lock.py", description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    lk = sub.add_parser("lock", help="resolve tools/alpine-packages.txt into the lock")
    lk.add_argument("--packages", type=Path, default=ROOT / "tools/alpine-packages.txt")
    lk.add_argument("--lock", type=Path, default=ROOT / "tools/alpine-packages.lock")
    lk.add_argument("--minirootfs", type=Path, default=ROOT / "build/deps/alpine-minirootfs")
    lk.add_argument("--apk-dir", type=Path, default=ROOT / "build/deps/alpine-packages")
    lk.add_argument("--mirror", default=DEFAULT_MIRROR)
    lk.add_argument("--repo", action="append", choices=DEFAULT_REPOS)
    lk.set_defaults(fn=cmd_lock)
    st = sub.add_parser("stage", help="extract every locked package's data into DEST")
    st.add_argument("--lock", type=Path, required=True)
    st.add_argument("--apk-dir", type=Path, required=True)
    st.add_argument("--minirootfs", type=Path, required=True)
    st.add_argument("dest", type=Path)
    st.set_defaults(fn=cmd_stage)
    sz = sub.add_parser(
        "size", help="print size_mib=, inodes=, content= and inodes_used= lines for STAGE"
    )
    sz.add_argument("stage", type=Path)
    sz.set_defaults(fn=cmd_size)
    fl = sub.add_parser("files", help="print the lock's `repo file sha256` lines")
    fl.add_argument("--lock", type=Path, default=ROOT / "tools/alpine-packages.lock")
    fl.set_defaults(fn=cmd_files)
    a = p.parse_args(argv)
    try:
        return a.fn(a)
    except (LockError, OSError, tarfile.TarError, zlib.error) as exc:
        print(f"alpine-lock: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
