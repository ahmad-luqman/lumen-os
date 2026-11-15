"""PCI devices: the virtio devices the harness attaches, and `lspci`.

Boots with every device `Devices.every()` names and checks QEMU's own view
of them: it starts without complaint (CI's QEMU accepts each one, sound and
its wav audiodev among them), `query-pci` lists each, the disk is the image
the harness made, the CPU model is `max`, the forwarded host port is open,
and the kernel still boots. And when a forwarded host port is taken,
real QEMU fails the way the harness expects and the retry recovers.

The kernel's side: `lspci` lists exactly the functions `query-pci` reports
(address, vendor and device ids), names the virtio ones, and the boot log
counts the memory BARs QEMU has; also behind a PCIe root port (on a
secondary bus);
each function is logged at debug level; its records filter (`where vendor =
0x1af4`); and without ACPI there is no MCFG, so no PCI devices, with a
warning and the reason in `lspci`'s header. The decoding itself is host-tested (`cargo test -p lumen-pci`,
`-p lumen-acpi` for the MCFG) and checked in-kernel
(`lumen_kernel::pci::*`).
"""

import re
import socket

import pytest
from lumen_harness import qemu
from lumen_harness.qemu import DEVICE_IDS, HOSTFWD_FAILED, Devices, Machine
from lumen_harness.screen import PROMPT

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
    assert m.qemu_complaints() == []
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
        assert m.qemu_complaints() == []
        with socket.create_connection(("127.0.0.1", m.forwards[80]), timeout=5):
            pass


# (Without ACPI there is no keyboard, so no shell on the screen.)
SHELLS = r"INFO  shell: shells? on serial.*; type 'help'\n"
PCI_LINE = re.compile(
    r"INFO  pci: (\d+) functions?, (\d+) memory BARs, through ECAM: "
    r"segment 0 buses 0-255 at 0x[0-9a-f]+0000000\n"
)
# What `lspci` calls each device the harness attaches.
VIRTIO_NAMES = {
    "tablet": "virtio input",
    "keyboard": "virtio input",
    "rng": "virtio rng",
    "sound": "virtio sound",
    "blk": "virtio block",
    "net": "virtio network",
    "share": "virtio 9p",
}


def shell(boot, **kwargs) -> Machine:
    m = boot("qemutest", **kwargs)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def run(m: Machine, line: str) -> str:
    """Run `line` in the serial shell: what it printed between its echo and
    the next prompt."""
    m.send_line(line)
    echo = m.expect(re.escape(line) + r"\n", 10)
    end = m.expect(PROMPT, 10)
    return m.output[echo.end() : end.start()]


def lspci(m: Machine) -> dict[str, tuple[int, int, str, str]]:
    """`lspci`'s rows by address: vendor, device, class and name. Checks the
    header says nothing is missing, and the ids' four hex digits."""
    out = run(m, "lspci")
    lines = out.splitlines()
    header = re.fullmatch(r"lspci: (\d+) functions?", lines[0])
    assert header, out
    assert re.fullmatch(r"  addr\s+vendor\s+device\s+class\s+name", lines[1]), out
    rows = {}
    for line in lines[2:]:
        addr, vendor, device, cls, name = re.split(r"\s{2,}", line.strip())
        assert re.fullmatch(r"0x[0-9a-f]{4}", vendor) and re.fullmatch(r"0x[0-9a-f]{4}", device), (
            line
        )
        rows[addr] = (int(vendor, 16), int(device, 16), cls, name)
    assert len(rows) == int(header[1]), out
    return rows


def pci_address(d: dict) -> str:
    return f"0000:{d['bus']:02x}:{d['slot']:02x}.{d['function']:x}"


def assert_lists_what_qemu_has(m: Machine, rows: dict) -> list[dict]:
    """`lspci`'s rows are exactly the functions QEMU has, with their ids;
    returns QEMU's."""
    qemu_devices = pci_devices(m)
    assert {a: r[:2] for a, r in rows.items()} == {
        pci_address(d): (d["id"]["vendor"], d["id"]["device"]) for d in qemu_devices
    }
    return qemu_devices


def test_lspci_lists_virtio_devices(boot):
    m = shell(boot, devices=Devices.every())
    rows = lspci(m)
    qemu_devices = assert_lists_what_qemu_has(m, rows)
    # Each device the harness attached is there under its name.
    ids = {qdev: key for key, qdev in DEVICE_IDS.items()}
    names = {
        ids[d["qdev_id"]]: rows[pci_address(d)][3] for d in qemu_devices if d["qdev_id"] in ids
    }
    assert names == VIRTIO_NAMES
    # Devices on q35's root bus keep virtio's transitional ids (0x1000-0x103f,
    # the type in the subsystem id), named all the same: if a QEMU default
    # changed that, this coverage would go quietly.
    assert any(v == VIRTIO_VENDOR and 0x1000 <= d <= 0x103F for v, d, *_ in rows.values())
    assert rows["0000:00:00.0"][2:] == ("host bridge", "Q35 host bridge")
    # The boot log counts them, and the memory BARs QEMU has (BARs 0-5:
    # QEMU lists the ROM as region 6).
    line = PCI_LINE.search(m.output)
    assert line, "no pci summary line"
    memory_bars = sum(
        1 for d in qemu_devices for r in d["regions"] if r["type"] == "memory" and r["bar"] < 6
    )
    assert (int(line[1]), int(line[2])) == (len(rows), memory_bars)
    assert "WARN  pci" not in m.output


def test_lspci_records_filter(boot):
    m = shell(boot, devices=Devices(rng=True, tablet=True, keyboard=True))
    rows = lspci(m)
    assert run(m, "lspci | where vendor = 0x1af4 | count") == "3\n"
    out = run(m, "lspci | where vendor = 0x1af4 | select name addr")
    picked = re.findall(r"^  (virtio \w+)\s+(\S+)$", out, re.MULTILINE)
    assert sorted(picked) == sorted(
        (name, addr) for addr, (vendor, _, _, name) in rows.items() if vendor == VIRTIO_VENDOR
    ), out
    assert sorted(name for name, _ in picked) == ["virtio input", "virtio input", "virtio rng"]
    # A hex column takes hex only.
    assert run(m, "lspci | where vendor = 6900 | count") == (
        "where: vendor is hex: write it with 0x (e.g. 0x20)\n"
    )
    out = run(m, "lspci | select addr class name")
    lines = out.splitlines()
    assert lines[0].split() == ["addr", "class", "name"], out
    assert [re.split(r"\s{2,}", line.strip()) for line in lines[1:]] == [
        [addr, cls, name] for addr, (_, _, cls, name) in rows.items()
    ], out
    bridges = sum(1 for _, _, cls, _ in rows.values() if "bridge" in cls)
    assert bridges >= 2  # the host bridge and the ISA bridge
    assert run(m, "lspci | where class ~ bridge | count") == f"{bridges}\n"


def test_lspci_sees_devices_behind_a_bridge(boot):
    # A PCIe root port with a virtio rng behind it, on a secondary bus: the
    # scan finds it without following the bridge (every bus is scanned).
    m = shell(
        boot,
        extra=[
            "-device", "pcie-root-port,id=lumen-rp1,bus=pcie.0,chassis=1",
            "-device", "virtio-rng-pci,id=lumen-rng-behind,bus=lumen-rp1",
        ],
    )  # fmt: skip
    rows = lspci(m)
    qemu_devices = assert_lists_what_qemu_has(m, rows)
    port = next(d for d in qemu_devices if d["qdev_id"] == "lumen-rp1")
    behind = next(d for d in qemu_devices if d["qdev_id"] == "lumen-rng-behind")
    assert behind["bus"] != 0
    # The root port's ids print with four digits: 0x000c, not 0x0c.
    assert rows[pci_address(port)] == (0x1B36, 0x000C, "PCI bridge", "PCIe root port")
    assert rows[pci_address(behind)][3] == "virtio rng"
    assert "WARN  pci" not in m.output


def test_each_function_is_logged_at_debug_level(boot):
    m = shell(boot, cmdline="lumen.log=debug")
    assert re.search(
        r"DEBUG pci: 0000:00:00\.0 8086:29c0 host bridge \(class 060000\) Q35 host bridge\n",
        m.output,
    ), m.output
    lines = re.findall(r"DEBUG pci: (\S+) ", m.output)
    assert lines == list(lspci(m)), m.output


def test_no_acpi_means_no_pci_devices(boot):
    # `acpi=off` leaves no MADT, so ACPI is absent and with it the MCFG.
    m = shell(boot, extra=["-machine", "q35,acpi=off"])
    assert "WARN  pci: no PCI devices: running without ACPI, so no MCFG\n" in m.output
    assert run(m, "lspci").startswith(
        "lspci: 0 functions (no PCI: running without ACPI, so no MCFG)\n"
    )
    # In a pipeline the reason goes to the console before the result.
    note = "lspci: no PCI: running without ACPI, so no MCFG\n"
    assert run(m, "lspci | count") == note + "0\n"
    out = run(m, "lspci | where vendor = 0x1af4")
    assert out.startswith(note) and out[len(note) :].split() == [
        "addr",
        "vendor",
        "device",
        "class",
        "name",
    ], out
