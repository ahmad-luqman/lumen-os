"""Sound: virtio-sound, `beep`, `play`, `volume` and `sound` (#99).

Each test boots with virtio-sound recorded through QEMU's wav audiodev
(`Devices(sound=True)`), plays something from the serial shell, waits until
`sound`'s records say it has been played, quits QEMU (so the wav is
complete) and measures what came out with the harness's audio helpers
(`lumen_harness.audio`, self-tested in `tests/harness/test_audio.py`). The
synthesis is host-tested (`cargo test -p lumen-synth`); choosing the stream's
format is a ktest (`lumen_kernel::sound::formats_are_chosen_from_what_the_stream_supports`).
"""

import re
import time

import pytest
from lumen_harness import audio
from lumen_harness.qemu import Devices, Machine
from lumen_harness.screen import PROMPT

SHELLS = r"INFO  shell: shells? on serial.*; type 'help'\n"
SOUND = Devices(sound=True)
# The driver's line once the device is set up.
READY = (
    r"INFO  sound: [0-9a-f:.]+: stream 0: signed 16-bit, (\d) channels?, (\d+) Hz, 20 ms periods\n"
)


def shell(boot, devices: Devices = SOUND) -> Machine:
    m = boot("qemutest", devices=devices)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def run(m: Machine, line: str, timeout: float = 10) -> str:
    """Run `line` in the serial shell: what it printed between its echo and
    the next prompt."""
    m.send_line(line)
    echo = m.expect(re.escape(line) + r"\n", 10)
    end = m.expect(PROMPT, timeout)
    return m.output[echo.end() : end.start()]


def sound_row(m: Machine) -> dict[str, str]:
    """`sound`'s one record, by column."""
    out = run(m, "sound | select state played frames underruns volume")
    header, row = out.splitlines()
    names = header.split()
    assert names == ["state", "played", "frames", "underruns", "volume"], out
    values = re.split(r"\s{2,}", row.strip())
    return dict(zip(names, values, strict=True))


def wait_played(m: Machine, n: int, timeout: float = 30) -> dict[str, str]:
    """Wait until `sound` says `n` sounds have been played and it is idle."""
    deadline = time.monotonic() + m._scaled(timeout)[0]
    while True:
        row = sound_row(m)
        if int(row["played"]) >= n and row["state"] == "idle":
            return row
        assert not row["state"].startswith("failed"), row
        assert time.monotonic() < deadline, f"still {row} after {timeout} s"
        time.sleep(0.2)


def recording(m: Machine) -> audio.Wav:
    """Quit QEMU (writing the wav out) and read what the guest played."""
    m.close()
    return audio.read_wav(m.wav_path)


def sounding(samples: list[int] | tuple[int, ...], level: int = 200) -> tuple[int, int]:
    """The first and last sample louder than `level`."""
    loud = [i for i, s in enumerate(samples) if abs(s) > level]
    assert loud, "nothing louder than the noise floor"
    return loud[0], loud[-1]


def test_tone_is_captured(boot):
    m = shell(boot)
    ready = re.search(READY, m.output)
    assert ready, "no sound line"
    assert run(m, "beep 440 500") == ""
    row = wait_played(m, 1)
    # 500 ms of samples, rounded up to whole 20 ms periods.
    rate = int(ready[2])
    assert 0 < int(row["frames"]) <= rate // 2 + rate // 50, row
    wav = recording(m)
    samples = wav.first_channel
    freq = audio.dominant_frequency(samples, wav.rate)
    assert freq is not None and abs(freq - 440) <= 4.4, freq
    first, last = sounding(samples)
    assert 0.45 <= (last - first) / wav.rate <= 0.55, (first, last, wav.rate)


def test_melody_notes_in_order(boot):
    m = shell(boot)
    # Blocks until the melody has played: the prompt comes back after it.
    assert run(m, "play t=120 w=sine C5/4 E5/4 G5/4 C6/4", timeout=30) == ""
    wait_played(m, 1)
    wav = recording(m)
    samples = wav.first_channel
    first, last = sounding(samples)
    notes = []
    for f in audio.window_frequencies(samples[first : last + 1], wav.rate, 0.1):
        if f is not None and (not notes or abs(f - notes[-1]) / notes[-1] > 0.03):
            notes.append(f)
    expected = [523.25, 659.26, 783.99, 1046.50]
    assert len(notes) == len(expected), notes
    for got, want in zip(notes, expected, strict=True):
        assert abs(got - want) <= want * 0.01, (notes, expected)
    # Each quarter note at tempo 120 lasts 500 ms.
    assert 1.9 <= (last - first) / wav.rate <= 2.1, (first, last)


def test_play_stops_on_q(boot):
    m = shell(boot)
    m.send_line("play t=60 C4/1 C4/1 C4/1 C4/1")
    m.expect(r"play t=60 C4/1 C4/1 C4/1 C4/1\n", 10)
    time.sleep(1)
    m.send("q")
    m.expect(r"play: stopped\n" + PROMPT, 10)
    row = wait_played(m, 1)
    # Far less than the melody's 16 s.
    assert int(row["frames"]) < 8 * 48_000, row


def test_volume_and_beep_from_rhai(boot):
    # Two beeps, the first at volume 100, the second (from a script) at 25:
    # at the second's pitch the recording is about a quarter as loud as at
    # the first's.
    m = shell(boot)
    assert run(m, "volume") == "volume: 50%\n"
    assert run(m, "volume 100") == "volume: 100%\n"
    assert run(m, "beep 440 300") == ""
    wait_played(m, 1)
    assert run(m, "volume 25") == "volume: 25%\n"
    assert run(m, 'rhai -e "beep(660, 300)"') == ""
    row = wait_played(m, 2)
    assert row["volume"] == "25%", row
    assert run(m, "volume 101") == "usage: volume [0-100]\n"
    wav = recording(m)
    samples = wav.first_channel
    loud = audio.tone_amplitude(samples, wav.rate, 440)
    quiet = audio.tone_amplitude(samples, wav.rate, 660)
    assert 3.0 <= loud / quiet <= 5.0, (loud, quiet)


def test_lspci_shows_sound_bound(boot):
    m = shell(boot)
    out = run(m, "lspci | where name = 'virtio sound' | select driver")
    assert out == "  driver\n  sound: bound\n", out
    out = run(m, "sound")
    assert re.fullmatch(
        r"sound: virtio sound at \S+, stream 0: signed 16-bit, \d channels?, \d+ Hz, 20 ms periods\n"
        r"  state\s+played\s+frames\s+underruns\s+volume\n"
        r"  idle\s+0\s+0\s+0\s+50%\n",
        out,
    ), out
    # (The crate itself warns that it got no jack or channel map infos:
    # QEMU's device has none.)
    assert "WARN  sound" not in m.output and "WARN  drivers" not in m.output


def test_no_sound_device_message(boot):
    m = shell(boot, devices=Devices())
    assert run(m, "beep 440 500") == "beep: no sound device\n"
    assert run(m, "play C4/4") == "play: no sound device\n"
    assert run(m, "sound") == "sound: no sound device\n"
    assert run(m, "sound | count") == "sound: no sound device\n0\n"
    assert run(m, 'rhai -e "beep(440, 100)"') == (
        "rhai: Runtime error: beep: no sound device (line 1, position 1)\n"
    )
    # Bad arguments are refused before the device is looked for.
    assert run(m, "beep 440 60001").startswith("usage: beep")
    assert run(m, "play").startswith("usage: play")


@pytest.mark.parametrize(
    ("line", "said"),
    [
        ("play C4/3", "play: length at 3 must be 1, 2, 4, 8, 16 or 32\n"),
        ("play C9/4", "play: octave at 1 is out of range (0-8, C0 lowest)\n"),
        (
            "beep 30000 100",
            "beep: 30000.000 Hz is too high for a 44100 Hz sample rate (it must be below half)\n",
        ),
    ],
)
def test_bad_sounds_are_refused(boot, line, said):
    m = shell(boot)
    assert run(m, line) == said
