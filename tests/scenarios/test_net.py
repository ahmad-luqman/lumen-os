"""M9 networking, socket, AF_UNIX, ICMP and epoll/socket stress scenarios (#471)."""

import re

from lumen_harness.gui import poll
from lumen_harness.qemu import EXIT_SUCCESS, Devices, require_user_network
from lumen_harness.shell import PROMPT, run

SHELLS = r"INFO  shell: shells? on serial.*; type 'help'\n"


def test_virtio_net_frames(boot):
    require_user_network()
    m = boot("qemutest", devices=Devices(net=True))
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    # QEMU's configured MAC is the independent control for config-space reads.
    mac = m.qmp().command("qom-get", path="/machine/peripheral/lumen-nic", property="mac")
    assert f"net: mac {mac}, link up, mtu 1500\n" in run(m, "net")
    counts = re.compile(
        r"net: rx (\d+), tx (\d+), rx_drops (\d+), tx_drops (\d+), "
        r"tx_free (\d+)/16, reclaimed (\d+), irqs (\d+)"
    )
    # More than the RX ring capacity: a missing repost must exhaust it.
    for _ in range(17):
        out = run(m, "net selftest", timeout=60)
        # This is slirp's MAC, not ours: an echoed request cannot be a success.
        assert out == "net selftest: ARP reply 10.0.2.2 is 52:55:0a:00:02:02\n", out

    # Wait for an observed effect, never assert a completion-time ceiling.
    def reclaimed_all(out):
        found = counts.search(out)
        assert found, out
        rx, tx, rx_drops, tx_drops, free, reclaimed, irqs = map(int, found.groups())
        assert rx >= 17 and tx == 17 and rx_drops == tx_drops == 0, out
        if free == 16:
            assert reclaimed == tx and irqs > 0, out
            return True
        return False

    out, done = poll(m, lambda: run(m, "net"), reclaimed_all, timeout=30)
    assert done, f"TX buffers never returned: {out}"
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
