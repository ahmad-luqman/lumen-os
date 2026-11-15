"""Reading wav files laid out as QEMU's wav audiodev writes them, and
measuring the pitch in them, for the sound scenarios. Pure Python (no
numpy): captures are a few seconds long.

QEMU writes the wav header with zero sizes when it opens the file and fills
them in only when the audiodev shuts down cleanly. A scenario's QEMU ends
through isa-debug-exit or is killed, so a data chunk size of 0 (or
0xFFFFFFFF, or more than the file holds) means "to the end of the file".
"""

from __future__ import annotations

import math
import struct
from collections.abc import Sequence
from dataclasses import dataclass
from itertools import pairwise
from pathlib import Path

_PCM = 1
_EXTENSIBLE = 0xFFFE
# KSDATAFORMAT_SUBTYPE_PCM, as stored in a WAVE_FORMAT_EXTENSIBLE fmt chunk.
_PCM_GUID = bytes.fromhex("0100000000001000800000aa00389b71")


@dataclass(frozen=True)
class Wav:
    rate: int
    channels: tuple[tuple[int, ...], ...]  # one tuple of signed 16-bit samples per channel

    def __post_init__(self) -> None:
        if self.rate <= 0:
            raise ValueError(f"sample rate must be positive, not {self.rate}")
        if not self.channels:
            raise ValueError("a wav has at least one channel")
        if len({len(c) for c in self.channels}) != 1:
            raise ValueError("every channel has the same number of samples")

    @property
    def first_channel(self) -> tuple[int, ...]:
        return self.channels[0]

    @property
    def duration(self) -> float:
        """Seconds of sound."""
        return len(self.first_channel) / self.rate


def read_wav(path: str | Path) -> Wav:
    return parse_wav(Path(path).read_bytes())


def parse_wav(data: bytes) -> Wav:
    """A 16-bit PCM wav (see the module comment for the data chunk's size);
    a partial frame at the end is dropped."""
    if len(data) < 12 or data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError("not a RIFF/WAVE file")
    pos = 12
    fmt: tuple[int, int] | None = None  # (channels, rate)
    while pos + 8 <= len(data):
        chunk, size = data[pos : pos + 4], struct.unpack_from("<I", data, pos + 4)[0]
        body = pos + 8
        if chunk == b"fmt ":
            fmt = _parse_fmt(data[body : body + size])
        elif chunk == b"data":
            if fmt is None:
                raise ValueError("data chunk before fmt chunk")
            channels, rate = fmt
            remaining = len(data) - body
            length = size if 0 < size <= remaining and size != 0xFFFFFFFF else remaining
            raw = data[body : body + length]
            raw = raw[: len(raw) - len(raw) % (2 * channels)]
            values = struct.unpack(f"<{len(raw) // 2}h", raw)
            return Wav(rate=rate, channels=tuple(values[c::channels] for c in range(channels)))
        pos = body + size + (size & 1)  # chunks are padded to even sizes
    raise ValueError("no data chunk")


def _parse_fmt(body: bytes) -> tuple[int, int]:
    if len(body) < 16:
        raise ValueError("truncated fmt chunk")
    tag, channels, rate, _, _, bits = struct.unpack_from("<HHIIHH", body)
    if tag == _EXTENSIBLE:
        if len(body) < 40 or body[24:40] != _PCM_GUID:
            raise ValueError("WAVE_FORMAT_EXTENSIBLE without the PCM subformat")
    elif tag != _PCM:
        raise ValueError(f"not PCM: format {tag:#x}")
    if bits != 16 or channels == 0 or rate == 0:
        raise ValueError(f"not 16-bit PCM: {bits} bits, {channels} channels, {rate} Hz")
    return channels, rate


def dominant_frequency(
    samples: Sequence[int],
    rate: int,
    *,
    threshold: float = 0.1,
    min_peak: int = 64,
    max_spread: float = 1.05,
) -> float | None:
    """The frequency of a single tone, from the times its signal rises
    through its mean (linearly interpolated between samples).

    A crossing counts only once the signal has dipped below `-threshold` of
    its peak (from the mean) and then risen above `+threshold`, so noise
    near the mean doesn't add crossings. None for silence (peak below
    `min_peak`), fewer than three crossings, or crossings that aren't
    evenly spaced (longest interval over `max_spread` times the shortest):
    two notes at once, a gap between notes, or noise have no single pitch.
    """
    if not samples:
        return None
    mean = sum(samples) / len(samples)
    peak = max(abs(v - mean) for v in samples)
    if peak < min_peak:
        return None
    level = threshold * peak
    times: list[float] = []
    armed = False
    pending: float | None = None
    prev = samples[0] - mean
    for i in range(1, len(samples)):
        v = samples[i] - mean
        if v < -level:
            armed, pending = True, None
        elif armed:
            if prev < 0 <= v:
                pending = i - 1 + (-prev) / (v - prev)
            if v > level and pending is not None:
                times.append(pending)
                armed, pending = False, None
        prev = v
    if len(times) < 3:
        return None
    intervals = [b - a for a, b in pairwise(times)]
    if max(intervals) > max_spread * min(intervals):
        return None
    return (len(times) - 1) * rate / (times[-1] - times[0])


def window_frequencies(samples: Sequence[int], rate: int, window: float) -> list[float | None]:
    """`dominant_frequency` of each `window`-second slice (the last, partial
    one included if it is at least half a window)."""
    if window <= 0:
        raise ValueError(f"window must be positive, not {window}")
    n = max(1, round(window * rate))
    out = []
    for start in range(0, len(samples), n):
        part = samples[start : start + n]
        if len(part) * 2 >= n:
            out.append(dominant_frequency(part, rate))
    return out


def tone_amplitude(samples: Sequence[int], rate: int, freq: float) -> float:
    """The amplitude of the `freq` component (Goertzel filter): about A for
    a sine of amplitude A at that frequency, near 0 for one well away from
    it. Compare it at the expected pitch and off it."""
    if not 0 < freq < rate / 2:
        raise ValueError(f"{freq} Hz is not between 0 and half of {rate} Hz")
    n = len(samples)
    if n == 0:
        return 0.0
    coeff = 2 * math.cos(2 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for v in samples:
        s1, s2 = v + coeff * s1 - s2, s1
    power = s1 * s1 + s2 * s2 - coeff * s1 * s2
    return 2 * math.sqrt(max(power, 0.0)) / n
