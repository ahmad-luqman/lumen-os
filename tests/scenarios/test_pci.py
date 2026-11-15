"""PCI devices: the virtio devices the harness attaches.

The kernel doesn't enumerate PCI yet (#94 adds that and `lspci`), so this
checks the harness side: QEMU starts with every virtio device attached (CI's
QEMU accepts each, virtio-sound and its wav audiodev among them), QMP's
`query-pci` lists them, the forwarded port is open on the host, and the
kernel still boots.
"""

import socket

from lumen_harness.qemu import Devices

# The `lumen-*` ids `device_args` gives the devices of `Devices.every()`.
EVERY_DEVICE = {
    "lumen-tablet",
    "lumen-keyboard",
    "lumen-rng",
    "lumen-sound",
    "lumen-blk",
    "lumen-nic",
    "lumen-9p",
}
VIRTIO_VENDOR = 0x1AF4


def pci_devices(m) -> list[dict]:
    """Every device on every bus QMP's `query-pci` reports."""
    found = []
    for bus in m.qmp().command("query-pci"):
        found += bus["devices"]
    return found


def test_boots_with_every_virtio_device(boot):
    m = boot("qemutest", devices=Devices.every(), cpu="max")
    m.expect(r"INFO  boot: boot complete", 60)
    virtio = {
        d["qdev_id"]: d["id"]["device"]
        for d in pci_devices(m)
        if d["id"]["vendor"] == VIRTIO_VENDOR
    }
    assert set(virtio) == EVERY_DEVICE, virtio
    assert m.blk_image.stat().st_size == 16 << 20
    assert m.share_dir.is_dir()
    # QEMU listens on the forwarded host port (user networking accepts the
    # connection on the host side even with nothing listening in the guest).
    with socket.create_connection(("127.0.0.1", m.forwards[80]), timeout=5):
        pass
