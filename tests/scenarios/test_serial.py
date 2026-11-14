"""The serial port: COM1 found at boot or not (`lumen-uart`'s scratch
register probe). The FIFO bursts and the probe itself are checked by the
ktests in `arch::serial` and by `cargo test -p lumen-uart`.
"""

from lumen_harness.qemu import EXIT_CRASH, ExpectTimeout

# Retyping the command this often until QEMU exits: keys pressed before the
# screen's shell runs may be lost or land in a half-typed line (Ctrl-U clears
# it first).
ATTEMPTS = 30
WAIT_PER_ATTEMPT = 2


def test_boots_without_a_uart(boot):
    # No serial port at all (`-serial none`: q35 creates no COM1): boot must
    # not hang or spin on a port that reads 0xff, and the screen's shell must
    # run. Without serial, the keyboard and the exit code are all there is:
    # `crash panic` typed there ends QEMU with the crash code.
    m = boot("qemutest", serial=False)
    for _ in range(ATTEMPTS):
        m.press(["ctrl", "u"])
        m.type_text("crash panic\n")
        try:
            code = m.wait_exit(WAIT_PER_ATTEMPT)
            break
        except ExpectTimeout:
            continue
    else:
        raise AssertionError(f"no crash after typing `crash panic` {ATTEMPTS} times")
    assert code == EXIT_CRASH
    # Nothing from the kernel came out of a port that isn't there (QEMU's
    # own warnings, such as KVM's about CPUID features, share the stream).
    assert "boot:" not in m.output and "lumen>" not in m.output, m.output
