"""Bounded-memory windowing for one finalized Recording.

A model's working set grows with its input length: one 423-second Recording
passed whole to Qwen3-ASR-0.6B peaked at 7,260 MiB. The worker therefore never
hands an adapter more than one window. A Recording is split into sequential,
non-overlapping windows of about 30 seconds, each window is transcribed on
its own with no text or state from any other window, and the window texts are
joined into one final Transcript only after every window succeeded.

Cut points prefer the quietest 200 ms frame inside the last 5 seconds before
the window limit so a word is not split; a region with no dip falls back to a
hard cut at the limit. A final remainder shorter than 1 second is folded into
the window before it (so one window may reach 31 seconds): a near-empty window
could be rejected by a model and fail the whole request. There is no overlap
and no text merging.

This module is model-agnostic and pure Python: it must not import a model
runtime, because every adapter's virtual environment loads it.
"""

from __future__ import annotations

import array
import itertools
import math
import operator
import sys
from dataclasses import dataclass
from typing import Callable

from .audio import SAMPLE_RATE, SAMPLE_WIDTH, validate_pcm16


WINDOW_SAMPLES = 30 * SAMPLE_RATE
SEARCH_SAMPLES = 5 * SAMPLE_RATE
# 200 ms frames are longer than a stop-consonant closure, so the quietest one
# is normally a pause between words rather than a gap inside a word.
FRAME_SAMPLES = SAMPLE_RATE // 5
HOP_SAMPLES = SAMPLE_RATE // 100
MAX_WINDOWS = 20
# The final window is never shorter than this unless it is the only window;
# folding it in makes the previous window at most WINDOW + MIN_FINAL - 1 samples.
MIN_FINAL_WINDOW_SAMPLES = SAMPLE_RATE
MAX_TRANSCRIPT_BYTES = 24 * 1024
# A cut is "quiet" when the quietest frame dips to at most 0.8 of the RMS of the
# loud (90th percentile) frames in the search region. Noisy dictation has
# shallow pauses (a real 62 s Recording measured ~0.55-0.75), so the ratio is
# loose; it only sends steady sound with no dip (tone, music, constant noise)
# to the hard cut.
QUIET_RATIO = 0.8


class WindowingError(ValueError):
    """A Recording cannot be windowed or joined within the fixed bounds."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code
        self.message = message


@dataclass(frozen=True)
class Window:
    """One half-open sample range ``[start, end)`` and how it ended."""

    start: int
    end: int
    cut: str  # "quiet", "hard", or "end" for the final window

    def pcm(self, pcm: bytes) -> bytes:
        return pcm[self.start * SAMPLE_WIDTH : self.end * SAMPLE_WIDTH]


def _samples(pcm: bytes) -> array.array:
    samples = array.array("h")
    samples.frombytes(pcm)
    if sys.byteorder != "little":
        samples.byteswap()
    return samples


def _quiet_cut(pcm: bytes, lo: int, hi: int) -> int | None:
    """Return the centre of the quietest frame in ``[lo, hi)``, or None.

    None means the region is shorter than one frame or has no clear dip.
    Among equally quiet frames the latest wins, keeping windows long.
    """

    if hi - lo < FRAME_SAMPLES:
        return None
    region = _samples(pcm[lo * SAMPLE_WIDTH : hi * SAMPLE_WIDTH])
    prefix = [0, *itertools.accumulate(map(operator.mul, region, region))]
    frames = [
        (math.sqrt((prefix[offset + FRAME_SAMPLES] - prefix[offset]) / FRAME_SAMPLES), offset)
        for offset in range(0, len(region) - FRAME_SAMPLES + 1, HOP_SAMPLES)
    ]
    quietest_rms, quietest_offset = min(frames, key=lambda frame: (frame[0], -frame[1]))
    loud = sorted(rms for rms, _ in frames)[(len(frames) * 9) // 10]
    if quietest_rms > QUIET_RATIO * loud:
        return None
    return lo + quietest_offset + FRAME_SAMPLES // 2


def plan_windows(
    pcm: bytes,
    *,
    window_samples: int = WINDOW_SAMPLES,
    search_samples: int = SEARCH_SAMPLES,
    max_windows: int = MAX_WINDOWS,
) -> list[Window]:
    """Split mono PCM16 into ordered, contiguous, non-overlapping windows.

    At most ``max_windows`` windows are produced. Every window is at most
    ``window_samples`` long, except that a final remainder shorter than
    ``MIN_FINAL_WINDOW_SAMPLES`` is folded into the window before it; folding
    only removes a window, so the cap still holds. A quiet cut is never placed
    so early that the remaining audio would need more than the window cap; near
    the cap (a 600-second Recording) every cut is a hard cut.
    """

    validate_pcm16(pcm)
    total = len(pcm) // SAMPLE_WIDTH
    if total > window_samples * max_windows:
        raise WindowingError(
            "too_many_windows",
            f"Recording needs more than {max_windows} windows of {window_samples} samples",
        )
    windows: list[Window] = []
    start = 0
    while total - start > window_samples:
        limit = start + window_samples
        remaining_windows = max_windows - len(windows) - 1
        earliest = max(limit - search_samples, total - remaining_windows * window_samples, start + 1)
        cut = _quiet_cut(pcm, earliest, limit)
        windows.append(Window(start, limit if cut is None else cut, "hard" if cut is None else "quiet"))
        start = windows[-1].end
    if windows and total - start < MIN_FINAL_WINDOW_SAMPLES:
        start = windows.pop().start
    windows.append(Window(start, total, "end"))
    return windows


def transcribe_windows(
    pcm: bytes,
    transcribe_window: Callable[[bytes], str],
    *,
    window_samples: int = WINDOW_SAMPLES,
) -> str:
    """Transcribe each window in order and join the non-empty texts.

    ``transcribe_window`` receives only its own window's bytes. An all-zero
    window is skipped without calling it. Any exception from a window
    propagates, so a failed window never yields a partial Transcript. An empty
    return value means that window had no speech and is dropped from the join.
    """

    texts: list[str] = []
    for window in plan_windows(pcm, window_samples=window_samples):
        window_pcm = window.pcm(pcm)
        if not window_pcm.strip(b"\0"):
            continue
        text = transcribe_window(window_pcm).strip()
        if text:
            texts.append(text)
    joined = " ".join(texts)
    if len(joined.encode("utf-8")) > MAX_TRANSCRIPT_BYTES:
        raise WindowingError(
            "transcript_too_large",
            f"joined Transcript exceeds {MAX_TRANSCRIPT_BYTES} bytes",
        )
    return joined
