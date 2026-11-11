"""STATUS.md must only say "passing" when every listed test ran and passed."""

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
