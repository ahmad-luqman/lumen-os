"""PCI devices: the virtio devices the harness attaches.

Boots with every device `Devices.every()` names and checks QEMU's own view
of them: it starts without complaint (CI's QEMU accepts each one, sound and
its wav audiodev among them), `query-pci` lists each, the disk is the image
the harness made, the CPU model is `max`, the forwarded host port is open,
and the kernel still boots.
"""

import re
import socket

from lumen_harness.qemu import DEVICE_IDS, Devices, Machine

EVERY_DEVICE = set(DEVICE_IDS.values())
# The PCI vendor id of every virtio device (Red Hat's).
VIRTIO_VENDOR = 0x1AF4
# A line QEMU itself prints (a warning or an error), not the guest.
QEMU_SAYS = re.compile(r"^\S*qemu-system-x86_64: .*$", re.MULTILINE)


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
    assert QEMU_SAYS.findall(m.output) == []
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
