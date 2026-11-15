"""The wav reader and pitch measurement the sound scenarios rely on."""

import math
import random
import struct

import pytest
from lumen_harness import audio

RATE = 48_000


def tone(freq: float, seconds: float, amp: int = 12_000, wave: str = "sine") -> list[int]:
    out = []
    for i in range(round(seconds * RATE)):
        phase = (freq * i / RATE) % 1.0
        s = math.sin(2 * math.pi * phase) if wave == "sine" else (1 if phase < 0.5 else -1)
        out.append(round(amp * s))
    return out


def wav_bytes(
    channels_samples: list[list[int]], rate: int = RATE, *, sizes: bool = True, bits: int = 16
) -> bytes:
    """A wav file; `sizes=False` leaves the RIFF and data sizes 0, as QEMU does
    until its audiodev shuts down cleanly."""
    channels = len(channels_samples)
    frames = zip(*channels_samples, strict=True)
    data = b"".join(struct.pack(f"<{channels}h", *f) for f in frames)
    fmt = struct.pack("<HHIIHH", 1, channels, rate, rate * channels * 2, channels * 2, bits)
    riff_size = 4 + 8 + len(fmt) + 8 + len(data) if sizes else 0
    data_size = len(data) if sizes else 0
    return (
        b"RIFF"
        + struct.pack("<I", riff_size)
        + b"WAVE"
        + b"fmt "
        + struct.pack("<I", len(fmt))
        + fmt
        + b"data"
        + struct.pack("<I", data_size)
        + data
    )


def within(got: float | None, want: float, fraction: float = 0.01) -> bool:
    return got is not None and abs(got - want) <= want * fraction


def test_440_hz_found_within_one_percent(tmp_path):
    path = tmp_path / "a.wav"
    path.write_bytes(wav_bytes([tone(440, 1.0)]))
    w = audio.read_wav(path)
    assert (w.rate, w.channels, len(w.samples)) == (RATE, 1, RATE)
    assert w.duration == pytest.approx(1.0)
    assert within(audio.dominant_frequency(w.samples, w.rate), 440)


def test_header_with_zero_sizes_reads_to_the_end_of_the_file():
    samples = tone(440, 0.5)
    w = audio.parse_wav(wav_bytes([samples], sizes=False))
    assert w.samples == samples
    assert within(audio.dominant_frequency(w.samples, w.rate), 440)


def test_partial_frame_at_the_end_is_dropped():
    data = wav_bytes([[1, 2, 3], [-1, -2, -3]], sizes=False) + b"\x07"
    assert audio.parse_wav(data).samples == [1, 2, 3]


def test_stereo_reads_the_first_channel():
    left, right = tone(440, 0.5), tone(1000, 0.5)
    w = audio.parse_wav(wav_bytes([left, right]))
    assert w.channels == 2 and w.samples == left


def test_square_waves_and_other_pitches():
    for freq in (110, 261.63, 440, 1000, 3000):
        for wave in ("sine", "square"):
            got = audio.dominant_frequency(tone(freq, 0.5, wave=wave), RATE)
            assert within(got, freq), (freq, wave, got)


def test_dc_offset_and_noise_do_not_add_crossings():
    rng = random.Random(1)
    samples = [v + 3000 + rng.randint(-800, 800) for v in tone(440, 0.5)]
    assert within(audio.dominant_frequency(samples, RATE), 440)


def test_silence_has_no_frequency():
    assert audio.dominant_frequency([0] * 4800, RATE) is None
    assert audio.dominant_frequency([5, -5] * 2400, RATE) is None  # below min_peak
    assert audio.dominant_frequency([], RATE) is None


def test_window_frequencies_follow_a_melody():
    samples = tone(440, 0.25) + [0] * (RATE // 4) + tone(660, 0.25) + tone(880, 0.25)
    got = audio.window_frequencies(samples, RATE, 0.25)
    assert len(got) == 4
    assert got[1] is None
    for g, want in zip([got[0], got[2], got[3]], [440, 660, 880], strict=True):
        assert within(g, want), got


def test_tone_amplitude_peaks_at_the_pitch():
    samples = tone(440, 0.5, amp=10_000)
    on = audio.tone_amplitude(samples, RATE, 440)
    off = audio.tone_amplitude(samples, RATE, 523.25)
    assert on == pytest.approx(10_000, rel=0.02)
    assert off < on / 20
    assert audio.tone_amplitude([], RATE, 440) == 0.0


@pytest.mark.parametrize(
    "data, message",
    [
        (b"", "RIFF"),
        (b"RIFX" + bytes(8), "RIFF"),
        (wav_bytes([[0, 0]], bits=8), "16-bit"),
        (b"RIFF" + bytes(4) + b"WAVE" + b"data" + bytes(4), "before fmt"),
        (b"RIFF" + bytes(4) + b"WAVE", "no data"),
    ],
)
def test_rejects_what_it_cannot_read(data, message):
    with pytest.raises(ValueError, match=message):
        audio.parse_wav(data)
