"""Reading QEMU's wav audiodev captures and measuring the pitch in them, for
the sound scenarios. Pure Python (no numpy): captures are a few seconds long.

QEMU writes the wav header with zero sizes when it opens the file and fills
them in only when the audiodev shuts down cleanly. A scenario's QEMU ends
through isa-debug-exit or is killed, so the header's sizes can't be trusted:
the samples run from the data chunk to the end of the file (QEMU writes no
chunk after it).
"""

from __future__ import annotations

import math
import struct
from dataclasses import dataclass
from pathlib import Path

_PCM = 1
_EXTENSIBLE = 0xFFFE


@dataclass
class Wav:
    rate: int
    channels: int
    samples: list[int]  # the first channel, signed 16-bit

    @property
    def duration(self) -> float:
        """Seconds of sound."""
        return len(self.samples) / self.rate


def read_wav(path: str | Path) -> Wav:
    return parse_wav(Path(path).read_bytes())


def parse_wav(data: bytes) -> Wav:
    """A 16-bit PCM wav's first channel. The data chunk's size is ignored
    (see the module comment); a partial frame at the end is dropped."""
    if len(data) < 12 or data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError("not a RIFF/WAVE file")
    pos = 12
    fmt: tuple[int, int, int] | None = None  # (channels, rate, bits)
    while pos + 8 <= len(data):
        chunk, size = data[pos : pos + 4], struct.unpack_from("<I", data, pos + 4)[0]
        body = pos + 8
        if chunk == b"fmt ":
            if size < 16 or body + 16 > len(data):
                raise ValueError("truncated fmt chunk")
            tag, channels, rate, _, _, bits = struct.unpack_from("<HHIIHH", data, body)
            if tag not in (_PCM, _EXTENSIBLE) or bits != 16 or channels == 0 or rate == 0:
                raise ValueError(f"not 16-bit PCM: format {tag:#x}, {bits} bits, {channels} ch")
            fmt = (channels, rate, bits)
        elif chunk == b"data":
            if fmt is None:
                raise ValueError("data chunk before fmt chunk")
            channels, rate, _ = fmt
            frame = 2 * channels
            raw = data[body:]
            raw = raw[: len(raw) - len(raw) % frame]
            values = struct.unpack(f"<{len(raw) // 2}h", raw)
            return Wav(rate=rate, channels=channels, samples=list(values[::channels]))
        pos = body + size + (size & 1)  # chunks are padded to even sizes
    raise ValueError("no data chunk")


def dominant_frequency(
    samples: list[int], rate: int, *, threshold: float = 0.1, min_peak: int = 64
) -> float | None:
    """The frequency of the strongest periodic signal, from the times of its
    rising zero crossings (linearly interpolated between samples). A
    crossing only counts once the signal has dipped below `-threshold` of
    its peak and then risen above `+threshold` (hysteresis), so noise and
    harmonics near zero don't add crossings. None for silence (peak below
    `min_peak`) or fewer than three crossings."""
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
    return (len(times) - 1) * rate / (times[-1] - times[0])


def window_frequencies(samples: list[int], rate: int, window: float) -> list[float | None]:
    """`dominant_frequency` of each `window`-second slice (the last, partial
    one included if it is at least half a window)."""
    n = max(1, round(window * rate))
    out = []
    for start in range(0, len(samples), n):
        part = samples[start : start + n]
        if len(part) * 2 >= n:
            out.append(dominant_frequency(part, rate))
    return out


def tone_amplitude(samples: list[int], rate: int, freq: float) -> float:
    """The amplitude of the `freq` component (Goertzel filter): about A for
    a sine of amplitude A at that frequency, near 0 for one well away from
    it. Compare it at the expected pitch and off it."""
    n = len(samples)
    if n == 0:
        return 0.0
    coeff = 2 * math.cos(2 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for v in samples:
        s1, s2 = v + coeff * s1 - s2, s1
    power = s1 * s1 + s2 * s2 - coeff * s1 * s2
    return 2 * math.sqrt(max(power, 0.0)) / n
