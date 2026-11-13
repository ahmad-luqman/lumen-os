"""Where each scenario keeps its artifacts."""

from pathlib import Path

import lumen_harness
from lumen_harness import REPO_ROOT, artifact_dir, artifacts_root, save_ktest_tap

ROOT = Path("/a")


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
    # module-keyed directory, not just the test name.
    assert artifacts.parts[-2:] == (
        "test_artifacts",
        "test_the_artifacts_fixture_uses_artifact_dir",
    )


def test_a_rerun_keeps_the_first_runs_ktest_tap(tmp_path, monkeypatch):
    # CI's TCG re-run (LUMEN_ARTIFACTS_DIR) must not overwrite the TAP the
    # first run's job summary reported.
    monkeypatch.setattr(lumen_harness, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(lumen_harness, "BUILD_DIR", tmp_path / "build")
    monkeypatch.delenv("LUMEN_ARTIFACTS_DIR", raising=False)
    first = save_ktest_tap("not ok 1 - first run\n")
    assert first == tmp_path / "build/test-results/ktest.tap"
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", "build/test-artifacts-tcg")
    rerun = save_ktest_tap("ok 1 - re-run\n")
    assert rerun == tmp_path / "build/test-artifacts-tcg/ktest.tap"
    assert first.read_text() == "not ok 1 - first run\n"
    assert rerun.read_text() == "ok 1 - re-run\n"


def test_artifacts_root_follows_lumen_artifacts_dir(monkeypatch):
    monkeypatch.delenv("LUMEN_ARTIFACTS_DIR", raising=False)
    assert artifacts_root() == lumen_harness.BUILD_DIR / "test-artifacts"
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", "")  # empty is unset
    assert artifacts_root() == lumen_harness.BUILD_DIR / "test-artifacts"
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", "build/test-artifacts-tcg")
    assert artifacts_root() == REPO_ROOT / "build/test-artifacts-tcg"


def test_the_ktest_scenario_saves_through_save_ktest_tap():
    # Its TAP, and the scenarios' artifacts, go through the functions that
    # know about re-runs (one reader of LUMEN_ARTIFACTS_DIR).
    ktest = (REPO_ROOT / "tests/scenarios/test_ktest.py").read_text()
    assert "save_ktest_tap(" in ktest and "ktest.tap" not in ktest
    conftest = (REPO_ROOT / "tests/conftest.py").read_text()
    assert "ARTIFACTS = artifacts_root()" in conftest and "LUMEN_ARTIFACTS_DIR\")" not in conftest
