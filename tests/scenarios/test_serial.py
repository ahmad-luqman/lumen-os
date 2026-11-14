"""The serial port: COM1 found at boot or not (`lumen-uart`'s probe). The
FIFO bursts and the probe itself are checked by the ktests in
`arch::x86_64::serial` and by `cargo test -p lumen-uart`.
"""

import time

from lumen_harness.image import assert_matches_golden, compare, crop, update_requested
from lumen_harness.qemu import EXIT_CRASH, ExpectTimeout
from PIL import Image

# The boot warning's row on the screen: the third log line, after the
# terminal's and the framebuffer's (the screen shows no timestamps). A new
# info line logged before it moves it, and this golden with it.
WARNING_ROW = {"origin": (0, 176), "size": (640, 16)}
BOOT_TIMEOUT = 60
# Retyping the command this often until QEMU exits: keys pressed before the
# screen's shell runs may be lost or land in a half-typed line (Ctrl-U clears
# it first).
ATTEMPTS = 30
WAIT_PER_ATTEMPT = 2


def _wait_for_row(m, golden):
    """Screendump until the warning's row matches `golden` (when
    re-recording: has something and holds still), or the boot timeout;
    the last screendump."""
    deadline = time.monotonic() + BOOT_TIMEOUT * m.timeout_scale
    previous = None
    while True:
        shot = m.screendump("screen.png")
        region = crop(shot, **WARNING_ROW)
        if update_requested():
            if region.getbbox() is not None and region == previous:
                return shot
            previous = region
        elif golden.exists() and compare(region, Image.open(golden), tolerance=0)[0].differing == 0:
            return shot
        if time.monotonic() > deadline:
            return shot
        time.sleep(1)


def test_boots_without_a_uart_and_warns_on_screen_golden(boot, artifacts, golden_dir):
    # No serial port at all (`-serial none`: q35 creates no COM1). The probe
    # finds none and the screen says so (the hazard it removes: an absent
    # port's line status says "data ready" forever, so the serial shell
    # would read 0xff without end). With no serial output to say when boot
    # got there, the screen is polled until the warning's row matches its
    # golden. Then the screen's shell runs: `crash panic` typed there ends
    # QEMU with the crash code.
    m = boot("qemutest", serial=False)
    golden = golden_dir / "no-serial-warning.png"
    shot = _wait_for_row(m, golden)
    assert_matches_golden(shot, golden, artifacts=artifacts, **WARNING_ROW)

    for _ in range(ATTEMPTS):
        m.press(["ctrl", "u"])
        m.type_text("crash panic\n")
        try:
            code = m.wait_exit(WAIT_PER_ATTEMPT)
            break
        except ExpectTimeout:
            pass
    else:
        raise AssertionError(f"no crash after typing `crash panic` {ATTEMPTS} times")
    assert code == EXIT_CRASH
    # The harness really ran without COM1: no kernel output arrived (QEMU's
    # own warnings, such as KVM's about CPUID features, share the stream).
    assert "boot:" not in m.output and "lumen>" not in m.output, m.output
