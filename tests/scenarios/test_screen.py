"""Framebuffer output, checked against golden images over QMP screendump."""

from lumen_harness.image import assert_matches_golden


def test_boot_hello_golden(boot, artifacts, golden_dir):
    m = boot("qemutest")
    m.expect(r"INFO  fb: framebuffer 1280x800 ready", 60)
    m.expect(r"lumen> ", 30)
    shot = m.screendump("boot.png")
    # Only the fixed header: the log lines below it may change with any commit.
    assert_matches_golden(
        shot, golden_dir / "boot-hello.png", origin=(0, 0), size=(640, 136), artifacts=artifacts
    )
