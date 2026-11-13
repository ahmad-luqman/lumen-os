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


def test_a_missing_ktest_tap_under_a_rerun_dir_is_explained(results_dir, monkeypatch, capsys):
    # LUMEN_ARTIFACTS_DIR set on a first run: the gate fails on the ktests,
    # and says why rather than only "no test ran with this id".
    monkeypatch.setenv("LUMEN_ARTIFACTS_DIR", "build/test-artifacts-tcg")
    assert status.run_status(write=False, check=True) == 1
    assert (
        "LUMEN_ARTIFACTS_DIR is set, so the ktest scenario saved its TAP in"
        in capsys.readouterr().err
    )
    monkeypatch.delenv("LUMEN_ARTIFACTS_DIR")
    assert status.run_status(write=False, check=True) == 1
    assert "LUMEN_ARTIFACTS_DIR" not in capsys.readouterr().err
