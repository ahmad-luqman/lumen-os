"""Golden-image comparison: tolerant of tiny noise, strict about real changes."""

import pytest
from lumen_harness.image import assert_matches_golden, compare
from PIL import Image


def solid(color, size=(64, 32)):
    return Image.new("RGB", size, color)


def test_identical_images_match():
    diff, _ = compare(solid((10, 20, 30)), solid((10, 20, 30)))
    assert diff.differing == 0


def test_small_channel_noise_is_tolerated():
    diff, _ = compare(solid((10, 20, 30)), solid((14, 20, 30)), tolerance=8)
    assert diff.differing == 0


def test_changed_region_is_counted():
    a = solid((0, 0, 0))
    b = a.copy()
    for x in range(10):
        b.putpixel((x, 0), (255, 255, 255))
    diff, mask = compare(a, b)
    assert diff.differing == 10
    assert diff.max_delta == 255
    assert mask.getpixel((0, 0)) == 255


def test_assert_matches_golden_writes_diff_artifacts(tmp_path, monkeypatch):
    monkeypatch.delenv("LUMEN_UPDATE_GOLDEN", raising=False)
    golden = tmp_path / "g.png"
    solid((0, 0, 0)).save(golden)
    shot = tmp_path / "shot.png"
    solid((255, 0, 0), (100, 100)).save(shot)
    with pytest.raises(AssertionError, match="pixels differ"):
        assert_matches_golden(shot, golden, (0, 0), (64, 32), tmp_path)
    assert (tmp_path / "g.actual.png").exists()
    assert (tmp_path / "g.diff.png").exists()


def test_missing_golden_fails_instead_of_passing(tmp_path, monkeypatch):
    monkeypatch.delenv("LUMEN_UPDATE_GOLDEN", raising=False)
    shot = tmp_path / "shot.png"
    solid((1, 2, 3), (100, 100)).save(shot)
    with pytest.raises(AssertionError, match="missing golden"):
        assert_matches_golden(shot, tmp_path / "none.png", (0, 0), (64, 32), tmp_path)
