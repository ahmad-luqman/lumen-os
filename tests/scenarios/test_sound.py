"""Sound: virtio-sound, `beep`, `play`, `volume` and `sound` (#99).

Most tests boot with virtio-sound recorded through QEMU's wav audiodev
(`Devices(sound=True)`) unless it is about having none, plays something
from the serial shell, waits until `sound`'s record says it has been played,
then quits QEMU (so the wav is complete) and measures what came out with
the harness's audio helpers (`lumen_harness.audio`, self-tested in
`tests/harness/test_audio.py`). The synthesis is host-tested (`cargo test
-p lumen-synth`); the stream's format choice and a failed or stuck device
are ktests (`lumen_kernel::sound::*`).
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
COLUMNS = ["state", "played", "replaced", "frames", "underruns", "volume"]


def shell(boot, devices: Devices = SOUND, **kwargs) -> Machine:
    m = boot("qemutest", devices=devices, **kwargs)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def rate_of(m: Machine) -> int:
    """The device's sample rate, from the driver's boot line."""
    ready = re.search(READY, m.output)
    assert ready, "no sound line"
    return int(ready[2])


def run(m: Machine, line: str, timeout: float = 10) -> str:
    """Run `line` in the serial shell: what it printed between its echo and
    the next prompt."""
    m.send_line(line)
    echo = m.expect(re.escape(line) + r"\n", 10)
    end = m.expect(PROMPT, timeout)
    return m.output[echo.end() : end.start()]


def sound_row(m: Machine) -> dict[str, str]:
    """`sound`'s one record, by column."""
    out = run(m, "sound | select " + " ".join(COLUMNS))
    header, row = out.splitlines()
    assert header.split() == COLUMNS, out
    return dict(zip(COLUMNS, re.split(r"\s{2,}", row.strip()), strict=True))


def wait_played(m: Machine, n: int, timeout: float = 30) -> dict[str, str]:
    """Wait until `sound` says `n` sounds have been played and it is idle."""
    deadline = time.monotonic() + timeout * m.timeout_scale
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


def assert_pitch(freq: float | None, want: float) -> None:
    """`freq` is `want` within 1%."""
    assert freq is not None and abs(freq - want) <= want * 0.01, (freq, want)


def loudness_ratio(wav: audio.Wav, loud_hz: float, quiet_hz: float) -> float:
    """How much louder the recording is at `loud_hz` than at `quiet_hz`."""
    samples = wav.first_channel
    return audio.tone_amplitude(samples, wav.rate, loud_hz) / audio.tone_amplitude(
        samples, wav.rate, quiet_hz
    )


def sounding(samples: list[int] | tuple[int, ...], level: int = 200) -> tuple[int, int]:
    """The first and last sample louder than `level`."""
    loud = [i for i, s in enumerate(samples) if abs(s) > level]
    assert loud, "nothing louder than the noise floor"
    return loud[0], loud[-1]


def test_tone_is_captured(boot):
    m = shell(boot)
    rate = rate_of(m)
    assert run(m, "beep 440 500") == ""
    row = wait_played(m, 1)
    # Exactly 500 ms of frames went to the device (25 whole periods, no
    # padding), with no underrun.
    assert int(row["frames"]) == rate // 2, row
    assert row["underruns"] == "0", row
    # Each period came back by its interrupt (#96).
    assert "interrupt never came" not in m.output
    wav = recording(m)
    samples = wav.first_channel
    assert_pitch(audio.dominant_frequency(samples, wav.rate), 440)
    first, last = sounding(samples)
    assert 0.45 <= (last - first) / wav.rate <= 0.55, (first, last, wav.rate)


def test_beep_returns_while_it_plays(boot):
    m = shell(boot)
    assert run(m, "beep 440 3000") == ""
    assert sound_row(m)["state"] == "playing"


def test_a_padded_last_period_counts_only_the_sound(boot):
    # 510 ms is 25.5 periods: the last one is half silence.
    m = shell(boot)
    rate = rate_of(m)
    assert run(m, "beep 440 510") == ""
    row = wait_played(m, 1)
    assert int(row["frames"]) == rate * 510 // 1000, row


def test_a_long_tone_plays_without_underrun(boot):
    # 500 periods: none found the queue empty, and all were taken back (a
    # run leaking per period would exhaust the run table long before).
    m = shell(boot)
    rate = rate_of(m)
    assert run(m, "beep 440 10000") == ""
    row = wait_played(m, 1, timeout=60)
    assert (row["frames"], row["underruns"]) == (str(rate * 10), "0"), row
    assert "interrupt never came" not in m.output


def test_melody_notes_in_order(boot):
    m = shell(boot)
    # Blocks until the melody has played: once the prompt is back, `sound`
    # says so without waiting.
    assert run(m, "play t=120 w=sine C5/4 E5/4 G5/4 C6/4", timeout=30) == ""
    row = sound_row(m)
    assert (row["state"], row["played"]) == ("idle", "1"), row
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
        assert_pitch(got, want)
    # Each quarter note at tempo 120 lasts 500 ms.
    assert 1.9 <= (last - first) / wav.rate <= 2.1, (first, last)


@pytest.mark.parametrize("key", ["q", "\x03"], ids=["q", "ctrl-c"])
def test_play_stops_on_a_key(boot, key):
    m = shell(boot)
    rate = rate_of(m)
    m.send_line("play t=60 C4/1 C4/1 C4/1 C4/1")
    m.expect(r"play t=60 C4/1 C4/1 C4/1 C4/1\n", 10)
    time.sleep(1)
    m.send(key)
    m.expect(r"play: stopped\n" + PROMPT, 10)
    row = wait_played(m, 1)
    # Far less than the melody's 16 s: about a second, plus the fade and the
    # periods already queued.
    assert int(row["frames"]) < 3 * rate, row


def test_a_newer_sound_replaces_the_one_playing(boot):
    m = shell(boot)
    rate = rate_of(m)
    assert run(m, "beep 440 5000") == ""
    time.sleep(0.5)
    assert run(m, "beep 660 300") == ""
    row = wait_played(m, 1)
    assert (row["played"], row["replaced"]) == ("1", "1"), row
    assert int(row["frames"]) < 2 * rate, row
    # The 440 Hz tone, then the 660 Hz one.
    wav = recording(m)
    windows = audio.window_frequencies(wav.first_channel, wav.rate, 0.1)
    pitches = [f for f in windows if f is not None]
    assert pitches, windows
    assert_pitch(pitches[0], 440)
    assert_pitch(pitches[-1], 660)


def test_a_sound_that_has_made_its_samples_plays_out(boot):
    # The first beep (150 ms, under the 160 ms queued) has made all its
    # samples when the second arrives 30 ms later: both play in full.
    m = shell(boot)
    assert run(m, 'rhai -e "beep(440, 150); sleep(30); beep(660, 150)"') == ""
    row = wait_played(m, 2)
    assert (row["played"], row["replaced"]) == ("2", "0"), row


def test_play_says_when_a_newer_sound_cut_it_off(boot):
    # The serial shell plays a melody; the screen's shell beeps meanwhile.
    m = shell(boot)
    m.send_line("play t=60 C4/1 C4/1")
    m.expect(r"play t=60 C4/1 C4/1\n", 10)
    time.sleep(1)
    m.type_text("beep 880 100\n")
    m.expect(r"play: interrupted by a newer sound\n" + PROMPT, 10)
    row = wait_played(m, 1)
    assert (row["played"], row["replaced"]) == ("1", "1"), row


def test_volume_and_beep_from_rhai(boot):
    # Two beeps, the first at volume 100, the second (from a script) at 25:
    # at the second's pitch the recording is about a quarter as loud as at
    # the first's.
    m = shell(boot)
    assert run(m, "volume") == "volume: 50%\n"
    assert run(m, "volume 100") == "volume: 100%\n"
    assert run(m, "beep 440 300") == ""
    wait_played(m, 1)
    assert run(m, "volume 25%") == "volume: 25%\n"
    assert run(m, 'rhai -e "beep(660, 300)"') == ""
    row = wait_played(m, 2)
    assert row["volume"] == "25%", row
    assert run(m, "volume 101").startswith("usage: volume [percent]")
    assert 3.0 <= loudness_ratio(recording(m), 440, 660) <= 5.0


def test_play_honours_the_volume(boot):
    m = shell(boot)
    run(m, "volume 100")
    assert run(m, "play w=square A4/4", timeout=30) == ""
    run(m, "volume 25")
    assert run(m, "play w=square E5/4", timeout=30) == ""
    wait_played(m, 2)
    assert 3.0 <= loudness_ratio(recording(m), 440, 659.26) <= 5.0


def test_lspci_shows_sound_bound(boot):
    m = shell(boot)
    out = run(m, "lspci | where name = 'virtio sound' | select driver")
    assert out == "  driver\n  sound: bound\n", out
    out = run(m, "sound")
    assert re.fullmatch(
        r"sound: virtio sound at \S+, stream 0: signed 16-bit, \d channels?, \d+ Hz, 20 ms periods\n"
        r"  state\s+played\s+replaced\s+frames\s+underruns\s+volume\n"
        r"  idle\s+0\s+0\s+0\s+0\s+50%\n",
        out,
    ), out
    # Its record filters like the others.
    assert run(m, "sound | where state = idle | count") == "1\n"
    # (The crate itself warns that it got no jack or channel map infos:
    # QEMU's device has none.)
    assert "WARN  sound" not in m.output and "WARN  drivers" not in m.output


def test_a_second_sound_device_is_declined(boot, tmp_path):
    # The player is one: the first sound function (in address order) is
    # bound, the second declined before its transport is touched, and a
    # beep plays on the first (recorded in `wav_path`).
    second = tmp_path / "second.wav"
    m = shell(
        boot,
        extra=[
            "-audiodev", f"wav,id=lumen-audio2,path={second}",
            "-device", "virtio-sound-pci,id=lumen-sound2,audiodev=lumen-audio2,streams=1",
        ],
    )  # fmt: skip
    out = run(m, "lspci | where name = 'virtio sound' | select driver")
    assert out == "  driver\n  sound: bound\n  no driver (sound declined it)\n", out
    declined = r"INFO  sound: \S+: another sound device is bound; Lumen plays on one\n"
    assert re.search(declined, m.output)
    assert len(re.findall(READY, m.output)) == 1
    assert run(m, "beep 440 300") == ""
    wait_played(m, 1)
    wav = recording(m)
    assert_pitch(audio.dominant_frequency(wav.first_channel, wav.rate), 440)


def test_no_sound_device_message(boot):
    m = shell(boot, devices=Devices())
    assert run(m, "beep 440 500") == "beep: no sound device\n"
    assert run(m, "play C4/4") == "play: no sound device\n"
    assert run(m, "sound") == "sound: no sound device\n"
    assert run(m, "sound | count") == "sound: no sound device\n0\n"
    assert run(m, 'rhai -e "beep(440, 100)"') == (
        "rhai: Runtime error: beep: no sound device (line 1, position 1)\n"
    )
    # Usage errors are refused first.
    assert run(m, "beep 440 60001").startswith("usage: beep")
    assert run(m, "play").startswith("usage: play")


def test_rhai_beep_defaults_and_ranges(boot):
    # `beep()` is 440 Hz for 200 ms; out-of-range arguments are refused.
    m = shell(boot)
    rate = rate_of(m)
    assert run(m, 'rhai -e "beep()"') == ""
    row = wait_played(m, 1)
    assert int(row["frames"]) == rate // 5, row
    assert run(m, 'rhai -e "beep(-1, 10)"') == (
        "rhai: Runtime error: beep: -1 Hz is out of range (0 to 4294967295) (line 1, position 1)\n"
    )
    assert run(m, 'rhai -e "beep(440, 60001)"') == (
        "rhai: Runtime error: beep: 60001 ms is out of range (0 to 60000) (line 1, position 1)\n"
    )
    assert run(m, "beep a b c").startswith("usage: beep")
    assert run(m, "beep 440 100 3").startswith("usage: beep")
    assert sound_row(m)["played"] == "1"


@pytest.mark.parametrize(
    ("line", "said"),
    [
        ("play C4/3", "play: length at 3 must be 1, 2, 4, 8, 16 or 32\n"),
        ("play C9/4", "play: octave at 1 is out of range (0-8, C0 lowest)\n"),
        (
            "beep 30000 100",
            "beep: 30000.000 Hz is too high for a {rate} Hz sample rate (it must be below half)\n",
        ),
    ],
)
def test_bad_sounds_are_refused(boot, line, said):
    m = shell(boot)
    assert run(m, line) == said.format(rate=rate_of(m))
    # Nothing plays, even a moment later.
    time.sleep(1)
    row = sound_row(m)
    assert (row["state"], row["played"], row["frames"]) == ("idle", "0", "0"), row
