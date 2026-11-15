"""The virtio device options and the CPU model: each one's command-line
fragment, the files and ports a `Machine` sets up for them, and a default
command line that doesn't change at all."""

import socket
from pathlib import Path

import pytest
from lumen_harness.qemu import (
    SHARE_TAG,
    Devices,
    Machine,
    QemuOptions,
    device_args,
    free_port,
    qemu_args,
)


def opts(**kw):
    return QemuOptions(iso=Path("x.iso"), vars_file=Path("v.fd"), accel="tcg", **kw)


def fragment(args: list[str], flag: str) -> list[str]:
    """Every value that follows `flag` in `args`."""
    return [args[i + 1] for i, a in enumerate(args) if a == flag]


@pytest.fixture
def pinned(monkeypatch):
    monkeypatch.setenv("LUMEN_QEMU", "qemu-system-x86_64")
    monkeypatch.setenv("LUMEN_OVMF_CODE", "/ovmf/CODE.fd")


def test_default_command_line_is_unchanged(pinned):
    # Byte for byte what every scenario ran before device options existed:
    # no device, no -cpu, no -audiodev, -netdev or -fsdev.
    assert qemu_args(opts()) == [
        "qemu-system-x86_64",
        "-machine", "q35",
        "-accel", "tcg",
        "-m", "256M",
        "-drive", "if=pflash,format=raw,readonly=on,file=/ovmf/CODE.fd",
        "-drive", "if=pflash,format=raw,file=v.fd",
        "-cdrom", "x.iso",
        "-boot", "d",
        "-serial", "stdio",
        "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",
        "-no-reboot",
        "-monitor", "none",
        "-display", "none",
        "-rtc", "base=utc",
    ]  # fmt: skip


def test_default_machine_command_line_is_unchanged(pinned, tmp_path, monkeypatch):
    monkeypatch.setenv("LUMEN_ACCEL", "tcg")
    m = Machine(Path("x.iso"), tmp_path)
    assert qemu_args(m.options(Path("v.fd"))) == [
        "qemu-system-x86_64",
        "-machine", "q35",
        "-accel", "tcg",
        "-m", "256M",
        "-drive", "if=pflash,format=raw,readonly=on,file=/ovmf/CODE.fd",
        "-drive", "if=pflash,format=raw,file=v.fd",
        "-cdrom", "x.iso",
        "-boot", "d",
        "-serial", "stdio",
        "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",
        "-no-reboot",
        "-monitor", "none",
        "-display", "none",
        "-qmp", f"unix:{m.qmp_path},server=on,wait=off",
        "-d", "cpu_reset,guest_errors", "-D", str(tmp_path / "qemu.log"),
        "-rtc", "base=utc",
    ]  # fmt: skip
    assert m.forwards == {}


def test_cpu_model():
    assert fragment(qemu_args(opts(cpu="max")), "-cpu") == ["max"]
    assert "-cpu" not in qemu_args(opts())


def test_cpu_model_through_machine(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, cpu="max")
    assert fragment(qemu_args(m.options(Path("v.fd"))), "-cpu") == ["max"]


@pytest.mark.parametrize(
    ("option", "device"),
    [
        ("virtio_tablet", "virtio-tablet-pci,id=lumen-tablet"),
        ("virtio_keyboard", "virtio-keyboard-pci,id=lumen-keyboard"),
        ("virtio_rng", "virtio-rng-pci,id=lumen-rng"),
    ],
)
def test_simple_virtio_devices(option, device):
    assert device_args(opts(**{option: True})) == ["-device", device]


def test_sound_records_to_a_wav_file():
    assert device_args(opts(sound_wav=Path("/a/audio.wav"))) == [
        "-audiodev", "wav,id=lumen-audio,path=/a/audio.wav",
        "-device", "virtio-sound-pci,id=lumen-sound,audiodev=lumen-audio",
    ]  # fmt: skip


def test_blk_uses_a_raw_image():
    assert device_args(opts(blk_image=Path("/a/disk.img"))) == [
        "-drive", "if=none,id=lumen-disk,format=raw,file=/a/disk.img",
        "-device", "virtio-blk-pci,id=lumen-blk,drive=lumen-disk",
    ]  # fmt: skip


def test_net_forwards_host_ports_on_loopback():
    assert device_args(opts(net_forwards=((5555, 80), (5556, 22)))) == [
        "-netdev",
        "user,id=lumen-net,hostfwd=tcp:127.0.0.1:5555-:80,hostfwd=tcp:127.0.0.1:5556-:22",
        "-device", "virtio-net-pci,id=lumen-nic,netdev=lumen-net,romfile=",
    ]  # fmt: skip


def test_net_without_forwards():
    assert fragment(device_args(opts(net_forwards=())), "-netdev") == ["user,id=lumen-net"]
    assert "-netdev" not in device_args(opts())


def test_9p_shares_a_folder():
    assert device_args(opts(share_dir=Path("/a/share"))) == [
        "-fsdev", "local,id=lumen-fs,path=/a/share,security_model=none",
        "-device", f"virtio-9p-pci,id=lumen-9p,fsdev=lumen-fs,mount_tag={SHARE_TAG}",
    ]  # fmt: skip


def test_commas_in_paths_are_escaped():
    # QEMU would read what follows a lone comma as more options.
    args = device_args(
        opts(sound_wav=Path("/a,b/w.wav"), blk_image=Path("/a,b/d.img"), share_dir=Path("/a,b/s"))
    )
    assert "wav,id=lumen-audio,path=/a,,b/w.wav" in args
    assert "if=none,id=lumen-disk,format=raw,file=/a,,b/d.img" in args
    assert "local,id=lumen-fs,path=/a,,b/s,security_model=none" in args


def test_devices_come_before_extra_args():
    args = qemu_args(opts(virtio_rng=True, extra=["-smp", "2"]))
    assert args[-4:] == ["-device", "virtio-rng-pci,id=lumen-rng", "-smp", "2"]


def test_machine_maps_devices_into_its_artifacts(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, devices=Devices.every())
    o = m.options(Path("v.fd"))
    assert (o.virtio_tablet, o.virtio_keyboard, o.virtio_rng) == (True, True, True)
    assert o.sound_wav == m.wav_path == tmp_path / "audio.wav"
    assert o.blk_image == m.blk_image == tmp_path / "disk.img"
    assert o.share_dir == m.share_dir == tmp_path / "share"
    assert o.net_forwards == ((m.forwards[80], 80),)


def test_machine_without_devices_attaches_none(tmp_path):
    assert device_args(Machine(Path("x.iso"), tmp_path).options(Path("v.fd"))) == []


def test_net_alone_has_no_forwards(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(net=True))
    assert m.options(Path("v.fd")).net_forwards == ()


def test_forwarded_ports_are_free_and_distinct(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(forward=(80, 22, 8080)))
    hosts = list(m.forwards.values())
    assert sorted(m.forwards) == [22, 80, 8080]
    assert len(set(hosts)) == 3
    for port in hosts:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.bind(("127.0.0.1", port))  # nothing holds it


def test_free_port_is_bindable():
    port = free_port()
    assert 0 < port < 65536
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", port))


def test_start_creates_the_disk_and_share(tmp_path, monkeypatch):
    exe = tmp_path / "fake-qemu"
    exe.write_text("#!/bin/sh\nexit 0\n")
    exe.chmod(0o755)
    vars_template = tmp_path / "VARS.fd"
    vars_template.write_bytes(b"vars")
    monkeypatch.setenv("LUMEN_QEMU", str(exe))
    monkeypatch.setenv("LUMEN_OVMF_VARS", str(vars_template))
    run = tmp_path / "run"
    with Machine(Path("x.iso"), run, devices=Devices(blk_mib=16, share=True)) as m:
        m.wait_exit(10)
    disk = run / "disk.img"
    assert disk.stat().st_size == 16 << 20
    assert (run / "share").is_dir() and not any((run / "share").iterdir())
    cmdline = (run / "cmdline.txt").read_text()
    assert f"file={disk}" in cmdline and f"path={run / 'share'}" in cmdline
