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
import shlex
import time
from pathlib import Path

import pytest
from lumen_harness import audio
from lumen_harness.klog import UNDERRUNS, unexpected_warnings
from lumen_harness.qemu import Devices, Machine
from lumen_harness.screen import PROMPT
from lumen_harness.shell import run

SHELLS = r"INFO  shell: shells? on serial.*; type 'help'\n"
SOUND = Devices(sound=True)
# The sound queue's depth and a period's length (kernel/src/sound.rs's DEPTH
# and PERIOD_MS): an underrun is counted only when every queued period came
# back in one pass, and the queue is refilled to its depth each pass, so a
# sound underruns at most once per DEPTH periods, however a host stalls it.
DEPTH = 8
PERIOD_MS = 20
# The driver's line once the device is set up.
READY = (
    r"INFO  sound: [0-9a-f:.]+: stream 0: signed 16-bit, (\d) channels?, (\d+) Hz, "
    rf"{PERIOD_MS} ms periods\n"
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


def most_underruns(rate: int, frames: int) -> int:
    """The most underruns a sound of `frames` frames can count."""
    period = rate * PERIOD_MS // 1000
    periods = -(-frames // period)  # rounded up: a padded last period counts
    return periods // DEPTH


def underruns_warned(m: Machine) -> int:
    """The underruns the kernel's warnings so far add up to (one warning per
    sound that ended with any, logged before it is counted: klog.UNDERRUNS,
    a loaded host's; a failed sound counts its own unwarned)."""
    return sum(int(found["n"]) for found in UNDERRUNS.finditer(m.output))


def sound_row(m: Machine) -> dict[str, str]:
    """`sound`'s one record, by column."""
    out = run(m, "sound | select " + " ".join(COLUMNS))
    header, row = out.splitlines()
    assert header.split() == COLUMNS, out
    return dict(zip(COLUMNS, shlex.split(row), strict=True))


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
    # padding); any underrun (a stalled host's) said in a warning.
    assert int(row["frames"]) == rate // 2, row
    assert row["underruns"] == str(underruns_warned(m)), (
        row,
        "underruns warned",
        underruns_warned(m),
    )
    assert int(row["underruns"]) <= most_underruns(rate, rate // 2), row
    # Each period came back by its interrupt (#96).
    assert "interrupt never came" not in m.output
    wav = recording(m)
    samples = wav.first_channel
    assert_pitch(audio.dominant_frequency(samples, wav.rate), 440)
    # At least the 500 ms played (a little less where the tone's first and
    # last periods are quiet); an underrun only adds silence in between.
    first, last = sounding(samples)
    assert (last - first) / wav.rate >= 0.45, (first, last, wav.rate)


def test_beep_returns_while_it_plays(boot):
    m = shell(boot)
    assert run(m, "beep 440 3000") == ""
    assert sound_row(m)["state"] == "playing"


def test_a_beep_shorter_than_the_queue_never_underruns(boot):
    # 140 ms is DEPTH - 1 periods: all queued, and the source found ended by
    # the next fill, before the first wait, so no pass can find the queue
    # empty unended, however a host stalls the thread (`most_underruns` is
    # 0: one underrun counted on every sound fails here). At DEPTH periods
    # the source is found ended only after the first wait.
    m = shell(boot)
    rate = rate_of(m)
    assert run(m, f"beep 440 {(DEPTH - 1) * PERIOD_MS}") == ""
    row = wait_played(m, 1)
    assert most_underruns(rate, int(row["frames"])) == 0, row
    assert row["underruns"] == "0", row


def test_the_bounds_constants_are_the_kernels():
    # DEPTH and PERIOD_MS as kernel/src/sound.rs has them (PERIOD_MS is also
    # in the driver's boot line, READY).
    src = (Path(__file__).resolve().parents[2] / "kernel/src/sound.rs").read_text()
    assert src.count(f"const DEPTH: usize = {DEPTH};") == 1
    assert src.count(f"const PERIOD_MS: u32 = {PERIOD_MS};") == 1


def test_a_padded_last_period_counts_only_the_sound(boot):
    # 510 ms is 25.5 periods: the last one is half silence.
    m = shell(boot)
    rate = rate_of(m)
    assert run(m, "beep 440 510") == ""
    row = wait_played(m, 1)
    assert int(row["frames"]) == rate * 510 // 1000, row


def test_a_long_tone_plays_every_period(boot):
    # 500 periods, all taken back (a run leaking per period would exhaust
    # the run table long before); any that found the queue empty (a
    # stalled host's underrun) counted and said in a warning.
    m = shell(boot)
    rate = rate_of(m)
    assert run(m, "beep 440 10000") == ""
    row = wait_played(m, 1, timeout=60)
    assert (row["frames"], row["underruns"]) == (str(rate * 10), str(underruns_warned(m))), row
    assert int(row["underruns"]) <= most_underruns(rate, rate * 10), row
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
    # Each quarter note at tempo 120 lasts 500 ms: at least 1.9 s sound (an
    # underrun, a stalled host's, only adds silence in between).
    assert (last - first) / wav.rate >= 1.9, (first, last)


@pytest.mark.parametrize("key", ["q", "\x03"], ids=["q", "ctrl-c"])
def test_play_stops_on_a_key(boot, key):
    m = shell(boot)
    rate = rate_of(m)
    start = time.monotonic()
    m.send_line("play t=60 C4/1 C4/1 C4/1 C4/1")
    m.expect(r"play t=60 C4/1 C4/1 C4/1 C4/1\n", 10)
    time.sleep(1)
    at_key = time.monotonic() - start
    m.send(key)
    m.expect(r"play: stopped\n" + PROMPT, 10)
    row = wait_played(m, 1)
    # Far less than the melody's 16 s: what played until the key was sent
    # (no more than the host's time since the command: the device takes
    # periods at QEMU's audio pace, which can't run ahead of the host's),
    # plus the key's way to the guest, the fade and the periods queued (2 s:
    # at risk on a host delaying serial input that long while the guest
    # plays on).
    assert int(row["frames"]) <= rate * (at_key + 2), (row, at_key)


def test_a_newer_sound_replaces_the_one_playing(boot):
    m = shell(boot)
    rate = rate_of(m)
    start = time.monotonic()
    assert run(m, "beep 440 5000") == ""
    time.sleep(0.5)
    at_second = time.monotonic() - start
    assert run(m, "beep 660 300") == ""
    row = wait_played(m, 1)
    assert (row["played"], row["replaced"]) == ("1", "1"), row
    # Far less than the first's 5 s: what it played until the second was sent
    # (no more than the host's time meanwhile, as above), the command's way
    # to the guest, the periods queued and a fade (2 s), and the second's
    # 300 ms.
    assert int(row["frames"]) <= rate * (at_second + 2 + 0.3), (row, at_second)
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
    # unconfirmed: the serial shell is busy playing, so no `kbd`; the
    # melody's answer, read next, confirms the beep came
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
    assert out == '  driver\n  "sound: bound"\n', out
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
    assert not (bad := unexpected_warnings(m.output, "sound")), "\n".join(bad)
    assert not (bad := unexpected_warnings(m.output, "drivers")), "\n".join(bad)


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
    assert out == '  driver\n  "sound: bound"\n  "no driver (sound declined it)"\n', out
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
