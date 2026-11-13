"""Where each scenario keeps its artifacts."""

import os
import subprocess
import sys
from pathlib import Path

import lumen_harness
import pytest
from lumen_harness import REPO_ROOT, artifact_dir, artifacts_root, save_ktest_tap

ROOT = Path("/a")


@pytest.fixture
def fake_repo(tmp_path, monkeypatch):
    """A repo root (returned) under tmp_path, so nothing is written to the real
    build/; LUMEN_ARTIFACTS_DIR starts unset."""
    monkeypatch.setattr(lumen_harness, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(lumen_harness, "BUILD_DIR", tmp_path / "build")
    monkeypatch.delenv("LUMEN_ARTIFACTS_DIR", raising=False)
    return tmp_path


def test_artifacts_are_keyed_by_module_and_test():
    # Same test name in two files: two directories (they may run at once).
    assert artifact_dir(ROOT, "tests/scenarios/test_boot.py::test_x") == ROOT / "test_boot/test_x"
    assert artifact_dir(ROOT, "tests/scenarios/test_mm.py::test_x") == ROOT / "test_mm/test_x"
    # Parameters are kept, made safe for a file name.
    assert artifact_dir(ROOT, "tests/scenarios/test_acpi.py::test_cpu[2,maxcpus=4-2 CPUs]") == (
        ROOT / "test_acpi/test_cpu_2_maxcpus_4-2_CPUs_"
    )


def test_the_artifacts_fixture_uses_artifact_dir(artifacts):
    # The fixture (tests/conftest.py) is what scenarios get: it must use the
    # module-keyed directory, not just the test name, under artifacts_root()
    # (a re-run's own root when LUMEN_ARTIFACTS_DIR is set).
    assert artifacts.parts[-2:] == (
        "test_artifacts",
        "test_the_artifacts_fixture_uses_artifact_dir",
    )
    assert artifacts.parents[1] == artifacts_root()


def test_a_rerun_keeps_the_first_runs_ktest_tap(fake_repo, monkeypatch):
    # CI's TCG re-run (LUMEN_ARTIFACTS_DIR) must not overwrite the TAP the
    # first run's job summary reported.
    tmp_path = fake_repo
    first = save_ktest_tap("not ok 1 - first run\n")
    assert first == tmp_path / "build/test-results/ktest.tap"
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", "build/test-artifacts-tcg")
    rerun = save_ktest_tap("ok 1 - re-run\n")
    assert rerun == tmp_path / "build/test-artifacts-tcg/ktest.tap"
    assert first.read_text() == "not ok 1 - first run\n"
    assert rerun.read_text() == "ok 1 - re-run\n"


def test_a_rerun_on_a_fresh_tree_writes_no_first_run_tap(fake_repo, monkeypatch):
    # Set on a first run, the variable leaves build/test-results/ktest.tap
    # unwritten (`make status` then says why: test_status).
    tmp_path = fake_repo
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", "rerun")
    assert save_ktest_tap("ok\n") == tmp_path / "rerun/ktest.tap"
    assert not (tmp_path / "build/test-results/ktest.tap").exists()


@pytest.mark.parametrize(
    ("name", "first_run"),
    [
        ("build/test-results", "test-results"),
        ("./build/test-results/", "test-results"),
        ("build/x/../test-results", "test-results"),
        ("BUILD/Test-Results", "test-results"),
        ("build/test-artifacts", "test-artifacts"),
        ("{root}/build/test-artifacts", "test-artifacts"),
    ],
)
def test_a_rerun_dir_may_not_be_the_first_runs(fake_repo, monkeypatch, name, first_run):
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", name.format(root=fake_repo))
    with pytest.raises(ValueError, match=f"is the first run's build/{first_run}: "):
        save_ktest_tap("ok\n")
    with pytest.raises(ValueError):
        artifacts_root()
    assert not (fake_repo / "build").exists()


def test_artifacts_root_follows_lumen_artifacts_dir(monkeypatch):
    monkeypatch.delenv("LUMEN_ARTIFACTS_DIR", raising=False)
    assert artifacts_root() == lumen_harness.BUILD_DIR / "test-artifacts"
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", "")  # empty is unset
    assert artifacts_root() == lumen_harness.BUILD_DIR / "test-artifacts"
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", "build/test-artifacts-tcg")
    assert artifacts_root() == REPO_ROOT / "build/test-artifacts-tcg"


def test_the_artifacts_fixture_follows_a_rerun_dir(tmp_path):
    # Run the fixture test above again in a pytest of its own, as CI's TCG
    # re-run does: with LUMEN_ARTIFACTS_DIR set, its artifacts go there.
    rerun = tmp_path / "rerun"
    env = {k: v for k, v in os.environ.items() if k != "PYTEST_ADDOPTS"}
    env["LUMEN_ARTIFACTS_DIR"] = str(rerun)
    node = "tests/harness/test_artifacts.py::test_the_artifacts_fixture_uses_artifact_dir"
    run = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", "-p", "no:xdist", node],
        cwd=REPO_ROOT,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert run.returncode == 0, run.stdout + run.stderr
    assert "1 passed" in run.stdout, run.stdout
    # A passing test's own directory is removed; its module's stays.
    assert (rerun / "test_artifacts").is_dir()


def test_the_ktest_scenario_saves_through_save_ktest_tap():
    # It writes no TAP of its own: only save_ktest_tap knows about re-runs.
    ktest = (REPO_ROOT / "tests/scenarios/test_ktest.py").read_text()
    assert "save_ktest_tap(" in ktest
    assert "write_text" not in ktest and "BUILD_DIR" not in ktest and "TAP_OUT" not in ktest
