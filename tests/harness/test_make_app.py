"""`make app NAME=foo` (#206; ARCHITECTURE section 8, "Building and
bundling"), in a temporary copy of the repository: it makes apps/foo from
apps/template with the crate renamed, and that copy builds into a module
the kernel would take (validated by wasmtime, `lumen` imports only,
`memory` fixed at the template's size, `lumen_main`, the template's
manifest). Names the kernel wouldn't embed, taken names and an existing
directory are refused, making nothing."""

import os
import shutil
import subprocess
import tomllib
from pathlib import Path

import pytest
import wasmtime
from lumen_harness import REPO_ROOT
from lumen_harness.wasm import compile_module, custom_section, engine

# apps/template/build.rs's MEMORY_BYTES, in 64 KiB pages.
TEMPLATE_PAGES = 4 * 1024 * 1024 // 65536


@pytest.fixture(scope="module")
def repo(tmp_path_factory) -> Path:
    """The repository's files (tracked, and new ones not ignored) copied
    into a temporary directory."""
    dest = tmp_path_factory.mktemp("repo")
    listed = subprocess.run(
        ["git", "ls-files", "-co", "--exclude-standard", "-z"],
        cwd=REPO_ROOT,
        capture_output=True,
        check=True,
    ).stdout.split(b"\0")
    for name in filter(None, listed):
        src = REPO_ROOT / os.fsdecode(name)
        if src.is_file():
            dst = dest / os.fsdecode(name)
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)
    return dest


def make_app(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["make", "--no-print-directory", "app", *args],
        cwd=repo,
        capture_output=True,
        text=True,
        check=False,
    )


def app_dirs(repo: Path) -> list[str]:
    return sorted(p.name for p in (repo / "apps").iterdir())


def test_a_new_app_is_the_template_renamed_and_builds(repo):
    made = make_app(repo, "NAME=foo")
    assert made.returncode == 0, made.stderr
    assert "apps/foo made from apps/template" in made.stdout, made.stdout
    manifest = tomllib.loads((repo / "apps" / "foo" / "Cargo.toml").read_text())
    assert manifest["package"]["name"] == "foo"
    assert manifest["lib"]["crate-type"] == ["cdylib"]
    # The sources are the template's: its title is its crate's name.
    for f in ("src/lib.rs", "build.rs"):
        assert (repo / "apps" / "foo" / f).read_bytes() == (
            REPO_ROOT / "apps" / "template" / f
        ).read_bytes()
    # Built like `make apps` builds it, sharing the real build's cache for
    # the registry crates (the copy's own crates build afresh).
    env = dict(os.environ, CARGO_TARGET_DIR=str(REPO_ROOT / "target-apps"))
    built = subprocess.run(
        ["cargo", "build", "--release", "-p", "foo"],
        cwd=repo / "apps",
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert built.returncode == 0, built.stderr
    data = (
        REPO_ROOT / "target-apps" / "wasm32-unknown-unknown" / "release" / "foo.wasm"
    ).read_bytes()
    module = compile_module(data, engine())
    assert {i.module for i in module.imports} == {"lumen"}
    exports = {e.name: e.type for e in module.exports}
    assert set(exports) == {"memory", "lumen_main"}
    memory = exports["memory"]
    assert isinstance(memory, wasmtime.MemoryType)
    assert memory.limits.min == memory.limits.max == TEMPLATE_PAGES
    assert custom_section(data, "lumen.manifest") == b"caps=desktop"


@pytest.mark.parametrize(
    ("args", "said"),
    [
        ((), "usage: make app NAME=<name>"),
        (("NAME=Foo",), "Foo: lower case letters, digits, - and _ only"),
        (("NAME=1x",), "1x: lower case letters, digits, - and _ only"),
        (("NAME=a.b",), "a.b: lower case letters, digits, - and _ only"),
        (("NAME=../x",), "../x: lower case letters, digits, - and _ only"),
        (("NAME=test-x",), "test-x: test- names are the runtime's test apps"),
        (("NAME=template",), "template: that name is taken"),
        (("NAME=testkit",), "testkit: that name is taken"),
        (("NAME=lumen-app",), "lumen-app: that name is taken"),
    ],
)
def test_a_name_the_kernel_would_refuse_or_a_taken_one_makes_nothing(repo, args, said):
    before = app_dirs(repo)
    made = make_app(repo, *args)
    assert made.returncode != 0
    assert f"make app: {said}" in made.stderr, made.stderr
    assert app_dirs(repo) == before


def test_an_existing_app_is_not_overwritten(repo):
    (repo / "apps" / "bar").mkdir()
    (repo / "apps" / "bar" / "mine.txt").write_text("keep")
    made = make_app(repo, "NAME=bar")
    assert made.returncode != 0
    assert "make app: apps/bar already exists" in made.stderr, made.stderr
    assert sorted(p.name for p in (repo / "apps" / "bar").iterdir()) == ["mine.txt"]
    shutil.rmtree(repo / "apps" / "bar")
