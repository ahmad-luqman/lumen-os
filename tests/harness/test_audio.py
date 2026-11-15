"""The wav reader and pitch measurement the sound scenarios rely on. The wav
files are handcrafted with QEMU's layout (header sizes 0 until a clean
shutdown)."""

import math
import random
import struct

import pytest
from lumen_harness import audio

RATE = 48_000


def tone(
    freq: float, seconds: float, amp: int = 12_000, wave: str = "sine", phase: float = 0.0
) -> list[int]:
    out = []
    for i in range(round(seconds * RATE)):
        p = (freq * i / RATE + phase) % 1.0
        s = math.sin(2 * math.pi * p) if wave == "sine" else (1 if p < 0.5 else -1)
        out.append(round(amp * s))
    return out


def lfsr_noise(samples: int, freq: int, amp: int = 12_000) -> list[int]:
    """The crate's noise waveform: a 16-bit Galois LFSR (taps 0xB400), one
    new bit per cycle of `freq`, held between."""
    state, out, phase = 0xACE1, [], 0.0
    for _ in range(samples):
        phase += freq / RATE
        if phase >= 1.0:
            phase -= 1.0
            state = (state >> 1) ^ (0xB400 if state & 1 else 0)
        out.append(amp if state & 1 else -amp)
    return out


def chunk(name: bytes, body: bytes, size: int | None = None) -> bytes:
    size = len(body) if size is None else size
    return name + struct.pack("<I", size) + body + (b"\0" if len(body) & 1 else b"")


def fmt_body(channels: int = 1, bits: int = 16, tag: int = 1, rate: int = RATE) -> bytes:
    return struct.pack("<HHIIHH", tag, channels, rate, rate * channels * 2, channels * 2, bits)


def wav_bytes(
    channels_samples: list[list[int]],
    *,
    sizes: bool = True,
    bits: int = 16,
    after: bytes = b"",
    fmt: bytes | None = None,
) -> bytes:
    """A wav file; `sizes=False` leaves the RIFF and data sizes 0, as QEMU does
    until its audiodev shuts down cleanly; `after` is appended after the data
    chunk."""
    frames = zip(*channels_samples, strict=True)
    data = b"".join(struct.pack(f"<{len(channels_samples)}h", *f) for f in frames)
    fmt = fmt_body(len(channels_samples), bits) if fmt is None else fmt
    body = b"WAVE" + chunk(b"fmt ", fmt) + chunk(b"data", data, None if sizes else 0) + after
    return b"RIFF" + struct.pack("<I", len(body) if sizes else 0) + body


def within(got: float | None, want: float, fraction: float = 0.01) -> bool:
    return got is not None and abs(got - want) <= want * fraction


def test_440_hz_found_within_one_percent(tmp_path):
    path = tmp_path / "a.wav"
    path.write_bytes(wav_bytes([tone(440, 1.0)]))
    w = audio.read_wav(path)
    assert (w.rate, len(w.channels), len(w.first_channel)) == (RATE, 1, RATE)
    assert w.duration == pytest.approx(1.0)
    assert within(audio.dominant_frequency(w.first_channel, w.rate), 440)


def test_header_with_zero_sizes_reads_to_the_end_of_the_file():
    samples = tone(440, 0.5)
    w = audio.parse_wav(wav_bytes([samples], sizes=False))
    assert list(w.first_channel) == samples


def test_a_chunk_after_the_data_is_not_read_as_samples():
    after = chunk(b"LIST", b"INFOISFT\x06\x00\x00\x00lumen\x00")
    w = audio.parse_wav(wav_bytes([[1, 2, 3]], after=after))
    assert w.first_channel == (1, 2, 3)


def test_partial_frame_at_the_end_is_dropped():
    data = wav_bytes([[1, 2, 3], [-1, -2, -3]], sizes=False) + b"\x07"
    w = audio.parse_wav(data)
    assert w.channels == ((1, 2, 3), (-1, -2, -3))


def test_stereo_exposes_every_channel():
    left, right = tone(440, 0.5), tone(1000, 0.5)
    w = audio.parse_wav(wav_bytes([left, right]))
    assert w.channels == (tuple(left), tuple(right))
    assert w.first_channel == tuple(left)


def test_wave_format_extensible_needs_the_pcm_subformat():
    def extensible(guid: bytes) -> bytes:
        return fmt_body(tag=0xFFFE) + struct.pack("<HHI", 22, 16, 0) + guid

    pcm = bytes.fromhex("0100000000001000800000aa00389b71")
    w = audio.parse_wav(wav_bytes([[5, 6]], fmt=extensible(pcm)))
    assert w.first_channel == (5, 6)
    float_guid = bytes.fromhex("0300000000001000800000aa00389b71")
    with pytest.raises(ValueError, match="PCM subformat"):
        audio.parse_wav(wav_bytes([[5, 6]], fmt=extensible(float_guid)))


@pytest.mark.parametrize("freq", [110, 261.63, 440, 1000, 3000])
@pytest.mark.parametrize("wave", ["sine", "square"])
def test_pitch_within_one_percent(freq, wave):
    got = audio.dominant_frequency(tone(freq, 0.5, wave=wave), RATE)
    assert within(got, freq), got


def test_noise_around_a_dc_offset_larger_than_the_tone():
    rng = random.Random(1)
    samples = [v + 20_000 + rng.randint(-300, 300) for v in tone(440, 0.5, amp=8_000)]
    assert within(audio.dominant_frequency(samples, RATE), 440)


def test_silence_has_no_frequency():
    assert audio.dominant_frequency([0] * 4800, RATE) is None
    assert audio.dominant_frequency([5, -5] * 2400, RATE) is None  # below min_peak
    assert audio.dominant_frequency([], RATE) is None


@pytest.mark.parametrize("split", [0.3, 0.5, 0.7])
def test_two_notes_in_one_window_have_no_single_pitch(split):
    # A4 then A5 averages to ~660 Hz, within 1% of E5: a plausible wrong note.
    samples = tone(440, 0.25 * split) + tone(880, 0.25 * (1 - split))
    assert audio.dominant_frequency(samples, RATE) is None


def test_a_chord_reports_its_fundamental():
    # A4 + A5 together repeat every A4 period: that is their pitch.
    a4, a5 = tone(440, 0.25, amp=8000), tone(880, 0.25, amp=8000, phase=0.1)
    got = audio.dominant_frequency([a + b for a, b in zip(a4, a5, strict=True)], RATE)
    assert within(got, 440)


def test_noise_has_no_pitch():
    for freq in (1000, 4000, 12_000):
        assert audio.dominant_frequency(lfsr_noise(RATE // 4, freq), RATE) is None, freq


def test_a_gap_between_notes_has_no_single_pitch():
    samples = tone(440, 0.1) + [0] * 2400 + tone(440, 0.1)
    assert audio.dominant_frequency(samples, RATE) is None


def test_window_frequencies_follow_a_melody():
    samples = tone(440, 0.25) + [0] * (RATE // 4) + tone(660, 0.25) + tone(880, 0.25)
    got = audio.window_frequencies(samples, RATE, 0.25)
    assert len(got) == 4
    assert got[1] is None
    for g, want in zip([got[0], got[2], got[3]], [440, 660, 880], strict=True):
        assert within(g, want), got
    with pytest.raises(ValueError, match="positive"):
        audio.window_frequencies(samples, RATE, 0)


def test_tone_amplitude_peaks_at_the_pitch():
    samples = tone(440, 0.5, amp=10_000)
    on = audio.tone_amplitude(samples, RATE, 440)
    off = audio.tone_amplitude(samples, RATE, 523.25)
    assert on == pytest.approx(10_000, rel=0.02)
    assert off < on / 20
    assert audio.tone_amplitude([], RATE, 440) == 0.0
    for bad in (0, -1, RATE / 2, RATE):
        with pytest.raises(ValueError, match="between 0 and half"):
            audio.tone_amplitude(samples, RATE, bad)


@pytest.mark.parametrize(
    "data, message",
    [
        (b"", "RIFF"),
        (b"RIFX" + bytes(8), "RIFF"),
        (wav_bytes([[0, 0]], bits=8), "16-bit"),
        (wav_bytes([[0, 0]], fmt=fmt_body(tag=3)), "not PCM"),
        (b"RIFF" + bytes(4) + b"WAVE" + chunk(b"fmt ", bytes(10)), "truncated fmt"),
        (b"RIFF" + bytes(4) + b"WAVE" + chunk(b"data", bytes(4)), "before fmt"),
        (b"RIFF" + bytes(4) + b"WAVE", "no data"),
    ],
)
def test_rejects_what_it_cannot_read(data, message):
    with pytest.raises(ValueError, match=message):
        audio.parse_wav(data)


@pytest.mark.parametrize(
    "rate, channels, message",
    [(0, ((1,),), "positive"), (RATE, (), "at least one"), (RATE, ((1, 2), (1,)), "same number")],
)
def test_wav_checks_its_fields(rate, channels, message):
    with pytest.raises(ValueError, match=message):
        audio.Wav(rate=rate, channels=channels)
