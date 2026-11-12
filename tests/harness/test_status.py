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
def one_feature(tmp_path, monkeypatch):
    """A features dir with one feature, and results with just its test."""
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


def junit(path, failed: bool):
    body = "<failure/>" if failed else ""
    path.write_text(
        f'<testsuite><testcase classname="tests.harness.test_x" name="t">{body}'
        "</testcase></testsuite>"
    )


@pytest.mark.parametrize(
    ("failed", "shown", "rc"), [(False, "passing", 0), (True, "**FAILING**", 1)]
)
def test_ci_job_summary_gets_the_table(one_feature, tmp_path, monkeypatch, failed, shown, rc):
    junit(one_feature / "harness.xml", failed)
    summary = tmp_path / "summary.md"
    summary.write_text("from an earlier step\n")
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    assert status.run_status(write=False, check=True) == rc
    text = summary.read_text()
    assert text.startswith("from an earlier step\n# Lumen status\n")
    assert f"| M1 | demo | Thing | {shown} | `harness:test_x::t` |" in text
