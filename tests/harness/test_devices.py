"""The virtio device options and the CPU model: each one's command-line
fragment, what a `Machine` sets up for them (files, forwarded ports, the
retry when a port is taken, a graceful quit with sound), the checks on
`Devices`, and a default command line that doesn't change at all."""

import dataclasses
import socket
import subprocess
from pathlib import Path

import pytest
from lumen_harness import qemu
from lumen_harness.qemu import (
    DEVICE_IDS,
    HOSTFWD_FAILED,
    NO_DEVICES,
    Devices,
    Machine,
    device_args,
    free_ports,
    qemu_args,
)
from qemu_fakes import fake_qemu, opts


def fragment(args: list[str], flag: str) -> list[str]:
    """Every value that follows `flag` in `args`."""
    return [args[i + 1] for i, a in enumerate(args) if a == flag]


@pytest.fixture
def pinned(monkeypatch):
    monkeypatch.setenv("LUMEN_QEMU", "qemu-system-x86_64")
    monkeypatch.setenv("LUMEN_OVMF_CODE", "/ovmf/CODE.fd")


def test_default_command_line_is_unchanged(pinned):
    # Byte for byte what `qemu_args` gave before device options existed:
    # isa-debug-exit is the only -device; no -cpu, -audiodev, -netdev or
    # -fsdev. (Scenarios add -qmp, -d and -D through Machine: next test.)
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
    assert m.devices is NO_DEVICES
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


def test_boot_fixture_passes_devices_and_cpu(boot, tmp_path, monkeypatch):
    # Scenarios boot through the fixture: its default must be no devices
    # (so their command line is unchanged), and what they ask for must
    # reach Machine.
    made = []

    class FakeMachine:
        def __init__(self, iso, artifacts, **kw):
            made.append(kw)

        def start(self):
            return self

        def close(self):
            pass

    iso = tmp_path / "x.iso"
    iso.touch()
    monkeypatch.setitem(boot.__globals__, "Machine", FakeMachine)
    monkeypatch.setitem(boot.__globals__["ISOS"], "qemutest", iso)
    boot("qemutest")
    boot("qemutest", devices=Devices(rng=True), cpu="max")
    assert made[0]["devices"] is NO_DEVICES and made[0]["cpu"] is None
    assert made[1]["devices"] == Devices(rng=True) and made[1]["cpu"] == "max"


def test_cpu_model(tmp_path):
    assert fragment(qemu_args(opts(cpu="max")), "-cpu") == ["max"]
    assert "-cpu" not in qemu_args(opts())
    m = Machine(Path("x.iso"), tmp_path, cpu="max")
    assert fragment(qemu_args(m.options(Path("v.fd"))), "-cpu") == ["max"]


def test_sound_records_to_a_wav_file():
    assert device_args(opts(sound_wav=Path("/a/audio.wav"))) == [
        "-audiodev", "wav,id=lumen-audio,path=/a/audio.wav",
        "-device", "virtio-sound-pci,id=lumen-sound,audiodev=lumen-audio,streams=1",
    ]  # fmt: skip


def test_net_forwards_host_ports_on_loopback():
    assert device_args(opts(net_forwards={80: 5555, 22: 5556})) == [
        "-netdev",
        "user,id=lumen-net,hostfwd=tcp:127.0.0.1:5555-:80,hostfwd=tcp:127.0.0.1:5556-:22",
        "-device", "virtio-net-pci,id=lumen-nic,netdev=lumen-net,romfile=",
    ]  # fmt: skip


def test_net_without_forwards():
    assert fragment(device_args(opts(net_forwards={})), "-netdev") == ["user,id=lumen-net"]
    assert "-netdev" not in device_args(opts())


def test_9p_shares_a_folder():
    assert device_args(opts(share_dir=Path("/a/share"))) == [
        "-fsdev", "local,id=lumen-fs,path=/a/share,security_model=none",
        "-device", "virtio-9p-pci,id=lumen-9p,fsdev=lumen-fs,mount_tag=lumen-share",
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


@pytest.mark.parametrize(
    ("devices", "expected"),
    [
        (Devices(tablet=True), ["-device", "virtio-tablet-pci,id=lumen-tablet"]),
        (Devices(keyboard=True), ["-device", "virtio-keyboard-pci,id=lumen-keyboard"]),
        (Devices(rng=True), ["-device", "virtio-rng-pci,id=lumen-rng"]),
        (
            Devices(sound=True),
            [
                "-audiodev", "wav,id=lumen-audio,path={art}/audio.wav",
                "-device", "virtio-sound-pci,id=lumen-sound,audiodev=lumen-audio,streams=1",
            ],
        ),
        (
            Devices(blk_mib=4),
            [
                "-drive", "if=none,id=lumen-disk,format=raw,file={art}/disk.img",
                "-device", "virtio-blk-pci,id=lumen-blk,drive=lumen-disk",
            ],
        ),
        (
            Devices(blk_image=Path("/keep/disk.img")),
            [
                "-drive", "if=none,id=lumen-disk,format=raw,file=/keep/disk.img",
                "-device", "virtio-blk-pci,id=lumen-blk,drive=lumen-disk",
            ],
        ),
        (
            Devices(net=True),
            [
                "-netdev", "user,id=lumen-net",
                "-device", "virtio-net-pci,id=lumen-nic,netdev=lumen-net,romfile=",
            ],
        ),
        (
            Devices(share=True),
            [
                "-fsdev", "local,id=lumen-fs,path={art}/share,security_model=none",
                "-device", "virtio-9p-pci,id=lumen-9p,fsdev=lumen-fs,mount_tag=lumen-share",
            ],
        ),
    ],
    ids=["tablet", "keyboard", "rng", "sound", "blk_mib", "blk_image", "net", "share"],
)  # fmt: skip
def test_one_device_attaches_exactly_its_own(tmp_path, devices, expected):
    m = Machine(Path("x.iso"), tmp_path, devices=devices)
    want = [a.replace("{art}", str(tmp_path)) for a in expected]
    assert device_args(m.options(Path("v.fd"))) == want


def test_every_turns_every_device_on():
    every = Devices.every()
    for f in dataclasses.fields(Devices):
        if f.name == "blk_image":  # the alternative to blk_mib, which is on
            continue
        assert getattr(every, f.name) != f.default, f.name
    assert set(DEVICE_IDS) == {"tablet", "keyboard", "rng", "sound", "blk", "net", "share"}


@pytest.mark.parametrize(
    "kw",
    [
        {"blk_mib": 0},
        {"blk_mib": -1},
        {"blk_mib": True},
        {"blk_mib": 1.5},
        {"blk_mib": 4, "blk_image": Path("/d.img")},
        {"forward": (80, 80)},
        {"forward": (0,)},
        {"forward": (70000,)},
        {"forward": ("80",)},
        {"forward": (True,)},
    ],
    ids=repr,
)
def test_devices_reject_bad_values(kw):
    with pytest.raises(ValueError):
        Devices(**kw)


def test_devices_normalize_forward_and_blk_image():
    d = Devices(forward=[80, 22], blk_image="/d.img")  # type: ignore[arg-type]
    assert d.forward == (80, 22) and d.blk_image == Path("/d.img")
    assert d.wants_net and Devices(net=True).wants_net and not NO_DEVICES.wants_net


def test_machine_maps_devices_into_its_artifacts(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, devices=Devices.every())
    m._pick_forwards()
    o = m.options(Path("v.fd"))
    assert (o.virtio_tablet, o.virtio_keyboard, o.virtio_rng) == (True, True, True)
    assert o.sound_wav == m.wav_path == tmp_path / "audio.wav"
    assert o.blk_image == m.blk_image == tmp_path / "disk.img"
    assert o.share_dir == m.share_dir == tmp_path / "share"
    assert o.net_forwards == m.forwards and set(m.forwards) == {80}


@pytest.mark.parametrize(
    ("attr", "hint"),
    [("wav_path", "sound=True"), ("blk_image", "blk_mib"), ("share_dir", "share=True")],
)
def test_paths_of_absent_devices_say_how_to_attach_them(tmp_path, attr, hint):
    with pytest.raises(AttributeError, match=hint):
        getattr(Machine(Path("x.iso"), tmp_path), attr)


def test_devices_cant_be_swapped_after_creation(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(forward=(80,)))
    with pytest.raises(AttributeError):
        m.devices = NO_DEVICES  # type: ignore[misc]


def test_net_alone_has_no_forwards(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(net=True))
    m._pick_forwards()
    assert m.options(Path("v.fd")).net_forwards == {}


def test_forwarded_ports_are_free_and_distinct(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(forward=(80, 22, 8080)))
    m._pick_forwards()
    hosts = list(m.forwards.values())
    assert sorted(m.forwards) == [22, 80, 8080]
    assert len(set(hosts)) == 3
    for port in hosts:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.bind(("127.0.0.1", port))  # nothing holds it


def test_free_ports_are_distinct():
    # Every socket stays bound until all are picked, so the OS can't hand
    # out one port twice; many at once shows it.
    assert len(set(free_ports(64))) == 64


@pytest.fixture
def fake_env(tmp_path, monkeypatch):
    vars_template = tmp_path / "VARS.fd"
    vars_template.write_bytes(b"vars")
    monkeypatch.setenv("LUMEN_OVMF_VARS", str(vars_template))

    def use(script: str) -> None:
        monkeypatch.setenv("LUMEN_QEMU", str(fake_qemu(tmp_path, script)))

    return use


def test_start_zeroes_the_scratch_disk_and_makes_the_share(tmp_path, fake_env):
    fake_env("exit 0")
    run = tmp_path / "run"
    run.mkdir()
    (run / "disk.img").write_bytes(b"old data")
    with Machine(Path("x.iso"), run, devices=Devices(blk_mib=16, share=True)) as m:
        m.wait_exit(10)
    disk = run / "disk.img"
    assert disk.stat().st_size == 16 << 20
    assert disk.read_bytes()[:8] == bytes(8)
    assert (run / "share").is_dir() and not any((run / "share").iterdir())
    cmdline = (run / "cmdline.txt").read_text()
    assert f"file={disk}" in cmdline and f"path={run / 'share'}" in cmdline


def test_start_keeps_an_existing_image(tmp_path, fake_env):
    fake_env("exit 0")
    image = tmp_path / "keep.img"
    image.write_bytes(b"written by the first boot")
    with Machine(Path("x.iso"), tmp_path / "run", devices=Devices(blk_image=image)) as m:
        m.wait_exit(10)
        assert m.blk_image == image
    assert image.read_bytes() == b"written by the first boot"
    assert not (tmp_path / "run" / "disk.img").exists()


def test_a_taken_forward_port_is_picked_again_once(tmp_path, fake_env, monkeypatch):
    # The first QEMU fails like QEMU does when a host port is taken; start()
    # picks new ports and starts again.
    count = tmp_path / "count"
    failed = tmp_path / "failed"
    fake_env(
        # (`--version` is environment.txt's probe, not a start.)
        'if [ "$1" = --version ]; then exit 0; fi; '
        f"echo x >> {count}; if [ ! -e {failed} ]; then touch {failed}; "
        f"echo \"qemu-system-x86_64: -netdev ...: {HOSTFWD_FAILED} 'tcp:...'\"; exit 1; fi; "
        "exit 0"
    )
    picks = iter([[40001], [40002]])
    monkeypatch.setattr(qemu, "free_ports", lambda n: next(picks))
    with Machine(Path("x.iso"), tmp_path / "run", devices=Devices(forward=(80,))) as m:
        assert m.wait_exit(10) == 0
        assert m.forwards == {80: 40002}
        assert HOSTFWD_FAILED not in m.output
    assert count.read_text().count("x") == 2
    assert "127.0.0.1:40002-:80" in (tmp_path / "run" / "cmdline.txt").read_text()


def test_a_second_port_failure_is_left_to_the_test(tmp_path, fake_env, monkeypatch):
    fake_env(f'echo "{HOSTFWD_FAILED}"; exit 1')
    picks = iter([[40001], [40002], [40003]])
    monkeypatch.setattr(qemu, "free_ports", lambda n: next(picks))
    with Machine(Path("x.iso"), tmp_path / "run", devices=Devices(forward=(80,))) as m:
        assert m.wait_exit(10) == 1
        assert m.forwards == {80: 40002}  # tried twice, not more
        assert HOSTFWD_FAILED in m.output


class RecordingQmp:
    """Records QMP commands; `on_command` runs for each."""

    def __init__(self, on_command=lambda: None):
        self.sent: list[str] = []
        self.on_command = on_command

    def command(self, name, **kw):
        self.sent.append(name)
        self.on_command()

    def close(self):
        pass


def test_close_quits_gracefully_with_sound(tmp_path):
    # With sound attached, close() sends QMP `quit` (so the wav backend
    # writes its samples out) and waits, before any kill.
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(sound=True))
    proc = subprocess.Popen(["sleep", "30"])
    qmp = RecordingQmp(on_command=proc.terminate)  # QEMU's quit, played by SIGTERM
    m.proc, m._qmp = proc, qmp  # type: ignore[assignment]
    m.close()
    assert qmp.sent == ["quit"]
    assert proc.returncode == -15  # ended by the quit, not killed (-9)


def test_close_kills_without_sound(tmp_path):
    m = Machine(Path("x.iso"), tmp_path)
    proc = subprocess.Popen(["sleep", "30"])
    qmp = RecordingQmp()
    m.proc, m._qmp = proc, qmp  # type: ignore[assignment]
    m.close()
    assert qmp.sent == [] and proc.returncode == -9
