"""Block device (`virtio-blk`) and block cache (`lumen-bcache`) scenarios."""

import re

from lumen_harness.calibration import assert_quiet_boot
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.shell import PROMPT, run

SHELLS = r"INFO  shell: shells? on serial.*; type 'help'\n"


def shell(boot, **kwargs):
    m = boot("qemutest", cmdline="lumen.qemu_exit", **kwargs)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def parse_blk_read(out: str) -> bytes:
    """Parse `blk read` hex dump lines (`0000: <hex>`) into raw bytes."""
    buf = bytearray()
    for line in out.splitlines():
        m = re.fullmatch(r"[0-9a-f]{4}: ([0-9a-f]+)", line.strip())
        assert m, f"unexpected blk read line: {line!r}\n{out}"
        buf.extend(bytes.fromhex(m[1]))
    return bytes(buf)


def test_read_write_sectors(boot):
    # Boot 1: 1 MiB scratch disk (2048 sectors of 512 bytes).
    m1 = shell(boot, devices=Devices(blk_mib=1))
    assert_quiet_boot(m1.output)
    disk = m1.blk_image

    info_out = run(m1, "blk")
    assert "blk: 1 device\n" in info_out, info_out
    assert "2048" in info_out and "rw" in info_out and "yes" in info_out, info_out
    assert run(m1, "blk | count") == "1\n"

    # Out-of-range read/write returns an error cleanly without WARN/ERROR log lines.
    assert "out of range" in run(m1, "blk read 0 2048 1")
    assert "out of range" in run(m1, "blk write 0 2048 deadbeef 1")

    # Multi-sector write (> 4 KiB: 10 sectors = 5120 bytes) at LBA 0 + 1 sector at LBA 17.
    pat0 = bytes.fromhex("cafebabe12345678")
    expected0 = (pat0 * (5120 // len(pat0)))[:5120]
    pat17 = bytes.fromhex("a55a9669")
    expected17 = (pat17 * (512 // len(pat17)))[:512]

    out_w0 = run(m1, "blk write 0 0 cafebabe12345678 10")
    assert out_w0 == "blk 0: wrote 10 sector(s) at lba 0\n", out_w0
    out_w17 = run(m1, "blk write 0 17 a55a9669 1")
    assert out_w17 == "blk 0: wrote 1 sector(s) at lba 17\n", out_w17

    assert run(m1, "blk flush 0") == "blk 0: flushed\n"

    assert parse_blk_read(run(m1, "blk read 0 0 10")) == expected0
    assert parse_blk_read(run(m1, "blk read 0 17 1")) == expected17

    # Verify MSI-X interrupts were delivered (and no missed-interrupt warnings).
    irqs = run(m1, "irqs | where use ~ msix")
    assert "msix " in irqs, irqs
    assert "interrupt never came" not in m1.output

    # Clean QEMU shutdown before checking host image and rebooting on it.
    m1.send_line("exit")
    assert m1.wait_exit(10) == EXIT_SUCCESS
    m1.close()

    # Host-side verification of backing file bytes.
    raw = disk.read_bytes()
    assert len(raw) == 1 << 20
    assert raw[0:5120] == expected0
    assert raw[17 * 512 : 18 * 512] == expected17

    # Boot 2: attach the exact same backing image and verify persistence across reboot.
    m2 = shell(boot, devices=Devices(blk_image=disk))
    assert_quiet_boot(m2.output)
    assert parse_blk_read(run(m2, "blk read 0 0 10")) == expected0
    assert parse_blk_read(run(m2, "blk read 0 17 1")) == expected17

    m2.send_line("exit")
    assert m2.wait_exit(10) == EXIT_SUCCESS
    m2.close()


def test_readonly_device_rejects_writes(boot, tmp_path):
    img = tmp_path / "ro.img"
    seed = (b"LUMEN-RO-SECTOR!" * 32)[:512]
    img.write_bytes(seed + b"\x00" * ((1 << 20) - 512))
    before = img.read_bytes()

    m = shell(boot, devices=Devices(blk_image=img, blk_readonly=True))
    assert_quiet_boot(m.output)
    info_out = run(m, "blk")
    assert "ro" in info_out, info_out
    assert parse_blk_read(run(m, "blk read 0 0 1")) == seed
    assert "read-only" in run(m, "blk write 0 0 deadbeef 1")

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()
    assert img.read_bytes() == before


def test_max_blk_devices_declines_fifth(boot, tmp_path):
    extra = []
    for i in range(1, 5):
        p = tmp_path / f"extra{i}.img"
        p.write_bytes(b"\x00" * (1 << 20))
        extra += [
            "-drive",
            f"if=none,id=extra-disk-{i},format=raw,file={p}",
            "-device",
            f"virtio-blk-pci,id=extra-blk-{i},drive=extra-disk-{i}",
        ]
    m = shell(boot, devices=Devices(blk_mib=1), extra=extra)
    assert_quiet_boot(m.output)
    assert re.search(
        r"INFO  drivers: 4 bound, 0 failed, 1 declined, 0 not probed \(\d+ driver rows?\)\n",
        m.output,
    )
    out = run(m, "lspci | where name = 'virtio block' | select driver")
    assert out.count("virtio-blk: bound") == 4, out
    assert out.count("no driver (virtio-blk declined it)") == 1, out
    assert run(m, "blk | count") == "4\n"

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    m.close()
