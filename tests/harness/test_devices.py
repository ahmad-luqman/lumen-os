"""The virtio device options and the CPU model: each one's command-line
fragment, what a `Machine` sets up for them (files, forwarded ports, the
retry when a port is taken, a graceful quit with sound), the checks on
`Devices`, and a default command line that doesn't change at all."""

import dataclasses
import socket
import subprocess
import threading
import time
from pathlib import Path

import pytest
from lumen_harness import qemu
from lumen_harness.qemu import (
    HOSTFWD_FAILED,
    NO_DEVICES,
    Devices,
    Machine,
    device_args,
    free_ports,
    qemu_args,
)
from lumen_harness.qmp import QmpError
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


def test_net_forwards_host_ports_on_loopback():
    assert device_args(opts(net_forwards={80: 5555, 22: 5556})) == [
        "-netdev",
        "user,id=lumen-net,hostfwd=tcp:127.0.0.1:5555-:80,hostfwd=tcp:127.0.0.1:5556-:22",
        "-device", "virtio-net-pci,id=lumen-nic,netdev=lumen-net,romfile=",
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
            Devices(rng_rate=(64, 20000)),
            ["-device", "virtio-rng-pci,id=lumen-rng,max-bytes=64,period=20000"],
        ),
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
    ids=["tablet", "keyboard", "rng", "rng_rate", "sound", "blk_mib", "blk_image", "net", "share"],
)  # fmt: skip
def test_one_device_attaches_exactly_its_own(tmp_path, devices, expected):
    m = Machine(Path("x.iso"), tmp_path, devices=devices)
    want = [a.replace("{art}", str(tmp_path)) for a in expected]
    assert device_args(m.options(Path("v.fd"))) == want


def test_every_turns_every_device_on():
    every = Devices.every()
    for f in dataclasses.fields(Devices):
        # blk_image: the alternative to blk_mib, which is on; rng_rate: a
        # limit on rng, which is on unlimited.
        if f.name in ("blk_image", "rng_rate"):
            continue
        assert getattr(every, f.name) != f.default, f.name


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
        {"forward": (65536,)},
        {"forward": ("80",)},
        {"forward": (True,)},
        {"rng_rate": (0, 1000)},
        {"rng_rate": (64,)},
        {"rng_rate": (64, -1)},
        {"rng_rate": (64, 1.5)},
    ],
    ids=repr,
)
def test_devices_reject_bad_values(kw):
    with pytest.raises(ValueError):
        Devices(**kw)


def test_devices_normalize_forward_and_blk_image(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    d = Devices(forward=[80, 22], blk_image="d.img")  # type: ignore[arg-type]
    assert d.forward == (80, 22) and d.blk_image == tmp_path / "d.img"
    Devices(forward=(1, 65535))  # the bounds themselves are ports
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
    with pytest.raises(dataclasses.FrozenInstanceError):
        NO_DEVICES.sound = True  # type: ignore[misc]


def test_net_alone_has_no_forwards(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(net=True))
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


def launches(count: Path) -> int:
    return count.read_text().count("x") if count.exists() else 0


# A fake QEMU's first lines: skip environment.txt's `--version` probe (not
# a start), and count each start in `count`.
def counting(count: Path) -> str:
    return f'if [ "$1" = --version ]; then exit 0; fi; echo x >> {count}; '


def test_a_taken_forward_port_is_picked_again_once(tmp_path, fake_env, monkeypatch):
    # The first QEMU fails like QEMU does when a host port is taken; start()
    # keeps that attempt's files, picks new ports and starts again.
    count = tmp_path / "count"
    failed = tmp_path / "failed"
    fake_env(
        counting(count) + f"if [ ! -e {failed} ]; then touch {failed}; echo attempt one; "
        f"echo \"qemu-system-x86_64: -netdev ...: {HOSTFWD_FAILED} 'tcp:...'\" >&2; exit 1; fi; "
        "exit 0"
    )
    picks = iter([[40001], [40002]])
    monkeypatch.setattr(qemu, "free_ports", lambda n: next(picks))
    run = tmp_path / "run"
    m = Machine(Path("x.iso"), run, devices=Devices(forward=(80,)))
    with pytest.warns(UserWarning, match="host 40001 for guest 80.*retried with host 40002"):
        m.start()
    try:
        assert m.wait_exit(10) == 0
        assert m.forwards == {80: 40002}
        assert HOSTFWD_FAILED not in m.output
    finally:
        m.close()
    assert launches(count) == 2
    assert "127.0.0.1:40002-:80" in (run / "cmdline.txt").read_text()
    assert "127.0.0.1:40001-:80" in (run / "cmdline-attempt1.txt").read_text()
    assert (run / "serial-attempt1.log").read_text() == "attempt one\n"
    assert HOSTFWD_FAILED in (run / "qemu-stderr-attempt1.log").read_text()
    assert (run / "serial.log").read_text() == ""


def test_a_second_port_failure_is_left_to_the_test(tmp_path, fake_env, monkeypatch):
    fake_env(f'echo "{HOSTFWD_FAILED}" >&2; exit 1')
    picks = iter([[40001], [40002], [40003]])
    monkeypatch.setattr(qemu, "free_ports", lambda n: next(picks))
    m = Machine(Path("x.iso"), tmp_path / "run", devices=Devices(forward=(80,)))
    with pytest.warns(UserWarning, match="retried"):
        m.start()
    try:
        assert m.wait_exit(10) == 1
        assert m.forwards == {80: 40002}  # tried twice, not more
        assert HOSTFWD_FAILED in m.output
    finally:
        m.close()


def test_other_failures_are_not_retried(tmp_path, fake_env):
    count = tmp_path / "count"
    fake_env(
        counting(count) + 'echo "qemu-system-x86_64: -device virtio-foo: not found" >&2; exit 1'
    )
    with Machine(Path("x.iso"), tmp_path / "run", devices=Devices(forward=(80,))) as m:
        assert m.wait_exit(10) == 1
        assert "virtio-foo: not found" in m.output
    assert launches(count) == 1


def test_a_forwarding_qemu_that_answers_qmp_starts_once(tmp_path, fake_env, monkeypatch):
    # The QMP connection that proves the forwards bound is only a probe:
    # it is closed, and later QMP calls get a connection with the usual
    # timeout, as without forwards.
    count = tmp_path / "count"
    fake_env(counting(count) + "exec sleep 30")
    clients = []

    class FakeClient:
        def __init__(self, timeout):
            self.timeout = timeout
            self.closed = False

        @classmethod
        def connect(cls, path, timeout=10):
            deadline = time.monotonic() + 5
            while launches(count) < 1 and time.monotonic() < deadline:
                time.sleep(0.01)  # answers once this QEMU is up
            clients.append(cls(timeout))
            return clients[-1]

        def close(self):
            self.closed = True

    monkeypatch.setattr(qemu, "QmpClient", FakeClient)
    with Machine(Path("x.iso"), tmp_path / "run", devices=Devices(forward=(80,))) as m:
        assert m._qmp is None
        assert m.qmp().timeout == Machine(Path("x.iso"), tmp_path).qmp().timeout == 10
    assert clients[0].closed
    assert launches(count) == 1


def test_qemu_stderr_is_kept_apart(tmp_path, fake_env):
    # QEMU's messages go to qemu-stderr.log and qemu_stderr(), the guest's
    # serial to serial.log; `output` (expect's view) has both.
    fake_env("echo from the guest; echo 'audio: from QEMU' >&2; exit 0")
    run = tmp_path / "run"
    with Machine(Path("x.iso"), run) as m:
        m.wait_exit(10)
        assert m.qemu_stderr() == "audio: from QEMU\n"
        assert "from the guest" in m.output and "audio: from QEMU" in m.output
    assert (run / "qemu-stderr.log").read_text() == "audio: from QEMU\n"
    assert (run / "serial.log").read_text() == "from the guest\n"


def test_complaints_leave_out_known_noise(tmp_path, fake_env):
    svm = (
        "qemu-system-x86_64: warning: host doesn't support requested feature: "
        "CPUID.80000001H:ECX.svm [bit 2]"
    )
    fake_env(f"echo \"{svm}\" >&2; echo 'audio: Can not open x' >&2; exit 0")
    with Machine(Path("x.iso"), tmp_path / "run") as m:
        m.wait_exit(10)
        assert svm in m.qemu_stderr()
        assert m.qemu_complaints() == ["audio: Can not open x"]


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


def sound_machine(tmp_path, proc, qmp) -> Machine:
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(sound=True))
    m.proc, m._qmp = proc, qmp  # type: ignore[assignment]
    return m


def test_close_waits_for_the_quit(tmp_path):
    proc = subprocess.Popen(["sleep", "30"])
    qmp = RecordingQmp(on_command=lambda: threading.Timer(0.3, proc.terminate).start())
    sound_machine(tmp_path, proc, qmp).close()
    assert proc.returncode == -15


def test_close_kills_when_quit_fails(tmp_path, monkeypatch):
    monkeypatch.setattr(Machine._quit, "__defaults__", (0.2,))
    proc = subprocess.Popen(["sleep", "30"])

    def broken():
        raise QmpError("QMP connection unusable")

    with pytest.warns(UserWarning) as said:
        sound_machine(tmp_path, proc, RecordingQmp(on_command=broken)).close()
    assert proc.returncode == -9
    # It still waited for QEMU to exit before the kill.
    assert [str(w.message).split(":")[0] for w in said] == [
        "QMP quit failed (QMP connection unusable)",
        "QEMU did not quit within 0.2s",
    ]
    assert "audio.wav may be truncated" in str(said[0].message)


def test_close_kills_when_quit_is_ignored(tmp_path, monkeypatch):
    monkeypatch.setattr(Machine._quit, "__defaults__", (0.2,))
    proc = subprocess.Popen(["sleep", "30"])
    qmp = RecordingQmp()
    with pytest.warns(UserWarning, match="did not quit within 0.2s"):
        sound_machine(tmp_path, proc, qmp).close()
    assert qmp.sent == ["quit"] and proc.returncode == -9


def test_close_after_exit_sends_no_quit(tmp_path):
    proc = subprocess.Popen(["true"])
    proc.wait()
    qmp = RecordingQmp()
    sound_machine(tmp_path, proc, qmp).close()
    assert qmp.sent == []


def test_close_kills_without_sound(tmp_path):
    m = Machine(Path("x.iso"), tmp_path)
    proc = subprocess.Popen(["sleep", "30"])
    qmp = RecordingQmp()
    m.proc, m._qmp = proc, qmp  # type: ignore[assignment]
    m.close()
    assert qmp.sent == [] and proc.returncode == -9


def test_machine_passes_the_rng_rate_on(tmp_path):
    o = Machine(Path("x.iso"), tmp_path, devices=Devices(rng_rate=[64, 20000])).options(
        Path("v.fd")
    )  # type: ignore[arg-type]
    assert o.virtio_rng_rate == (64, 20000) and not o.virtio_rng


def test_make_run_attaches_virtio_rng(tmp_path, monkeypatch):
    # So `random` works in `make run` (the default CPU has no RDRAND).
    import argparse

    from lumen_harness import __main__ as cli

    ran: list[list[str]] = []
    monkeypatch.setattr(cli, "BUILD_DIR", tmp_path)
    monkeypatch.setattr(cli, "fresh_vars", lambda dest: tmp_path / "vars.fd")
    monkeypatch.setattr(cli.subprocess, "call", lambda args: ran.append(args) or 0)
    assert cli.cmd_run(argparse.Namespace(iso=tmp_path / "x.iso", headless=True)) == 0
    assert "virtio-rng-pci,id=lumen-rng" in ran[0]
