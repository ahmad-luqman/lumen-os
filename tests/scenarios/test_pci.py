"""PCI devices: the virtio devices the harness attaches.

Boots with every device `Devices.every()` names and checks QEMU's own view
of them: it starts without complaint (CI's QEMU accepts each one, sound and
its wav audiodev among them), `query-pci` lists each, the disk is the image
the harness made, the CPU model is `max`, the forwarded host port is open,
and the kernel still boots. And when a forwarded host port is taken,
real QEMU fails the way the harness expects and the retry recovers.
"""

import socket

import pytest
from lumen_harness import qemu
from lumen_harness.qemu import DEVICE_IDS, HOSTFWD_FAILED, Devices, Machine

EVERY_DEVICE = set(DEVICE_IDS.values())
# The PCI vendor id of every virtio device (Red Hat's).
VIRTIO_VENDOR = 0x1AF4


def pci_devices(m: Machine) -> list[dict]:
    """Every device QMP's `query-pci` reports, behind bridges too."""

    def walk(devices: list[dict]) -> list[dict]:
        found = []
        for d in devices:
            found.append(d)
            found += walk(d.get("pci_bridge", {}).get("devices", []))
        return found

    return [d for bus in m.qmp().command("query-pci") for d in walk(bus["devices"])]


def test_boots_with_every_virtio_device(boot):
    # cpu="max" too: the CPU model option is checked here like the devices.
    m = boot("qemutest", devices=Devices.every(), cpu="max")
    m.expect(r"INFO  boot: boot complete", 60)
    virtio = {d["qdev_id"] for d in pci_devices(m) if d["id"]["vendor"] == VIRTIO_VENDOR}
    assert virtio == EVERY_DEVICE
    assert m.qemu_stderr() == ""
    # QEMU creates the wav file when it realizes the device.
    assert m.wav_path.exists()
    disks = {b["device"]: b for b in m.qmp().command("query-block")}
    assert disks["lumen-disk"]["inserted"]["file"] == str(m.blk_image)
    cpu = m.qmp().command("query-cpus-fast")[0]["qom-path"]
    assert m.qmp().command("qom-get", path=cpu, property="type") == "max-x86_64-cpu"
    # QEMU listens on the forwarded host port (user networking accepts the
    # connection on the host side even with nothing listening in the guest).
    with socket.create_connection(("127.0.0.1", m.forwards[80]), timeout=5):
        pass


def test_a_taken_host_port_is_picked_again(boot, monkeypatch):
    # Pins HOSTFWD_FAILED to what QEMU really prints: the first pick is a
    # port something already listens on, so QEMU exits with it, and start()
    # picks a free one and boots.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as taken:
        taken.bind(("127.0.0.1", 0))
        taken.listen()
        port = taken.getsockname()[1]
        real = qemu.free_ports
        picks = iter([[port]])
        monkeypatch.setattr(qemu, "free_ports", lambda n: next(picks, None) or real(n))
        with pytest.warns(UserWarning, match=f"host {port} for guest 80"):
            m = boot("qemutest", devices=Devices(forward=(80,)))
        m.expect(r"INFO  boot: boot complete", 60)
        assert m.forwards[80] != port
        assert HOSTFWD_FAILED in (m.artifacts / "qemu-stderr-attempt1.log").read_text()
        assert m.qemu_stderr() == ""
        with socket.create_connection(("127.0.0.1", m.forwards[80]), timeout=5):
            pass
