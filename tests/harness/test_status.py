"""STATUS.md must only say "passing" when every listed test ran and passed."""

import pytest
from lumen_harness import status
from lumen_harness.status import feature_status, pattern_matches


def test_brackets_are_literal_and_star_globs():
    assert pattern_matches("scenario:t::f[df]", "scenario:t::f[df]")
    assert not pattern_matches("scenario:t::f[df]", "scenario:t::fd")
    assert pattern_matches("scenario:t::f[*]", "scenario:t::f[pf]")
    assert pattern_matches("harness:test_tap::*", "harness:test_tap::test_x")


def test_feature_status():
    results = {"a:x::1": True, "a:x::2": True, "a:y::1": False}
    assert feature_status(["a:x::*"], results) == "pass"
    assert feature_status(["a:x::*", "a:y::1"], results) == "fail"
    assert feature_status(["a:x::1", "a:z::*"], results) == "missing"
    assert feature_status(["a:z::*"], {}) == "missing"


@pytest.fixture
def results_dir(tmp_path, monkeypatch):
    """One feature (`harness:test_x::t`) and an empty results dir (returned) for
    the test to fill."""
    features = tmp_path / "features"
    features.mkdir()
    (features / "demo.toml").write_text(
        '[[feature]]\nname = "Thing"\nmilestone = "M1"\ntests = ["harness:test_x::t"]\n'
    )
    results = tmp_path / "results"
    results.mkdir()
    monkeypatch.setattr(status, "FEATURES_DIR", features)
    monkeypatch.setattr(status, "RESULTS", results)
    # Under CI, run_status would append these fake tables to the job summary.
    monkeypatch.delenv("GITHUB_STEP_SUMMARY", raising=False)
    return results


@pytest.mark.parametrize(
    ("body", "shown", "rc"),
    [("", "passing", 0), ("<failure/>", "**FAILING**", 1)],
    ids=["passing", "failing"],
)
def test_ci_job_summary_gets_the_table(results_dir, tmp_path, monkeypatch, body, shown, rc):
    (results_dir / "harness.xml").write_text(
        f'<testsuite><testcase classname="tests.harness.test_x" name="t">{body}'
        "</testcase></testsuite>"
    )
    summary = tmp_path / "summary.md"
    summary.write_text("from an earlier step\n")
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    assert status.run_status(write=False, check=True) == rc
    text = summary.read_text()
    assert text.startswith("from an earlier step\n# Lumen status\n")
    assert f"| M1 | demo | Thing | {shown} | `harness:test_x::t` |" in text


@pytest.fixture
def ktest_feature(results_dir):
    """A second feature, listing a ktest (`ktest:k::t`)."""
    (results_dir.parent / "features/k.toml").write_text(
        '[[feature]]\nname = "Kernel thing"\nmilestone = "M1"\ntests = ["ktest:k::t"]\n'
    )
    return results_dir


def status_hint(capsys) -> list[str]:
    """The lines run_status printed about ktest.tap."""
    assert status.run_status(write=False, check=True) == 1
    return [line for line in capsys.readouterr().err.splitlines() if "ktest.tap" in line]


def test_a_missing_ktest_tap_is_explained(ktest_feature, monkeypatch, capsys):
    monkeypatch.delenv("LUMEN_ARTIFACTS_DIR", raising=False)
    assert status_hint(capsys) == [
        f"status: no {ktest_feature / 'ktest.tap'}: the ktest scenario did not run"
    ]


def test_a_ktest_tap_saved_in_a_rerun_dir_is_named(ktest_feature, tmp_path, monkeypatch, capsys):
    # LUMEN_ARTIFACTS_DIR set on a first run: the ktest scenario saved its TAP
    # in the re-run's directory, which status never reads.
    rerun = tmp_path / "rerun"
    rerun.mkdir()
    (rerun / "ktest.tap").write_text("ok 1 - k::t\n")
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", str(rerun))
    [hint] = status_hint(capsys)
    assert f"but there is {rerun / 'ktest.tap'}: LUMEN_ARTIFACTS_DIR keeps" in hint, hint


def test_the_hint_survives_an_invalid_rerun_dir(ktest_feature, monkeypatch, capsys):
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", "build/test-results")
    [hint] = status_hint(capsys)
    assert hint.endswith(": the ktest scenario did not run"), hint


def test_no_hint_with_a_ktest_tap_or_no_failing_ktest_feature(results_dir, monkeypatch, capsys):
    monkeypatch.delenv("LUMEN_ARTIFACTS_DIR", raising=False)
    # Only the harness feature fails: the ktests are not the problem.
    assert status_hint(capsys) == []
    (results_dir.parent / "features/k.toml").write_text(
        '[[feature]]\nname = "Kernel thing"\nmilestone = "M1"\ntests = ["ktest:k::t"]\n'
    )
    (results_dir / "ktest.tap").write_text("TAP version 13\n1..1\nnot ok 1 - k::t\n")
    assert status_hint(capsys) == []
