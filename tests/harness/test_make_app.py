"""`make app NAME=foo` (#206; ARCHITECTURE section 8, "Building and
bundling"), in a temporary copy of the repository: it makes apps/foo from
apps/template with the crate renamed, and that copy builds into a module
the kernel would take (validated by wasmtime, `lumen` imports only,
`memory` fixed at the template's size, `lumen_main`, the template's
manifest). Names the kernel wouldn't embed, taken names (a test app's
module name among them) and an existing directory are refused, making
nothing."""

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
    """Every entry of apps/, dot ones too (a half-made copy would be one)."""
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
    # Built as `make apps` builds every crate (`cargo build --release`, here
    # with `-p foo`), into the copy's own target directory (nothing left in
    # the real one).
    target = repo / "target-apps"
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    built = subprocess.run(
        ["cargo", "build", "--release", "-p", "foo"],
        cwd=repo / "apps",
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert built.returncode == 0, built.stderr
    data = (target / "wasm32-unknown-unknown" / "release" / "foo.wasm").read_bytes()
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
        (("NAME=template",), "apps/template already exists"),
        (("NAME=testkit",), "apps/testkit already exists"),
        (("NAME=lumen-app",), "apps/lumen-app already exists"),
        # A leading `-` or `_` would break cargo for the whole workspace.
        (("NAME=-x",), "-x: lower case letters, digits, - and _ only"),
        (("NAME=_x",), "_x: lower case letters, digits, - and _ only"),
        (("NAME=test_x",), "test_x: test- names are the runtime's test apps"),
        # The same crate once `-` and `_` are alike: cargo's output file
        # would be another crate's.
        (("NAME=lumen_app",), "lumen_app: that name is taken (apps/lumen-app)"),
        (("NAME=test_window",), "test_window: test- names are the runtime's test apps"),
        # A package of apps/Cargo.lock: the workspace's path dependencies,
        # what they pull in, and registry crates (`cargo build -p` would be
        # ambiguous).
        (("NAME=lumen-heap",), "lumen-heap: that name is taken (a package of apps/Cargo.lock)"),
        (
            ("NAME=lumen_app_api",),
            "lumen_app_api: that name is taken (a package of apps/Cargo.lock)",
        ),
        (("NAME=lumen-gfx",), "lumen-gfx: that name is taken (a package of apps/Cargo.lock)"),
        (("NAME=lumen_ui",), "lumen_ui: that name is taken (a package of apps/Cargo.lock)"),
        (("NAME=serde",), "serde: that name is taken (a package of apps/Cargo.lock)"),
        # A test app's module name (apps/test-hello is `hello`): the kernel
        # refuses a name in both sets.
        (("NAME=hello",), "hello: a test app's name (apps/test-hello)"),
        (("NAME=window",), "window: a test app's name (apps/test-window)"),
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


def test_a_copy_that_fails_leaves_nothing_behind(repo):
    # The rename fails (a template whose package isn't named `template`):
    # the half-made copy, in its dot directory, is removed.
    manifest = repo / "apps" / "template" / "Cargo.toml"
    good = manifest.read_text()
    before = app_dirs(repo)
    try:
        manifest.write_text(good.replace('name = "template"', 'name = "other"'))
        made = make_app(repo, "NAME=baz")
    finally:
        manifest.write_text(good)
    assert made.returncode != 0
    assert "make app: apps/baz/Cargo.toml: couldn't rename its package" in made.stderr, made.stderr
    assert app_dirs(repo) == before


def test_a_lock_file_naming_no_package_fails_closed(repo):
    lock = repo / "apps" / "Cargo.lock"
    good = lock.read_text()
    before = app_dirs(repo)
    try:
        lock.write_text("# empty\n")
        made = make_app(repo, "NAME=qux")
    finally:
        lock.write_text(good)
    assert made.returncode != 0
    assert "make app: apps/Cargo.lock names no package" in made.stderr, made.stderr
    assert app_dirs(repo) == before
