"""Audio and health-phrase helpers for the local worker."""

from __future__ import annotations

import array
import re
import wave
from pathlib import Path


SAMPLE_RATE = 16_000
CHANNELS = 1
SAMPLE_WIDTH = 2


class AudioError(ValueError):
    """The worker received audio outside its fixed PCM contract."""


def validate_pcm16(pcm: bytes) -> None:
    if not isinstance(pcm, bytes):
        raise AudioError("PCM payload must be bytes")
    if not pcm:
        raise AudioError("PCM payload is empty")
    if len(pcm) % SAMPLE_WIDTH:
        raise AudioError("PCM payload has an odd byte count")


def pcm_to_float32(pcm: bytes) -> list[float]:
    validate_pcm16(pcm)
    samples = array.array("h")
    samples.frombytes(pcm)
    if samples.itemsize != SAMPLE_WIDTH:
        raise AudioError("host signed-short width is not 16-bit")
    if samples and __import__("sys").byteorder != "little":
        samples.byteswap()
    return [sample / 32768.0 for sample in samples]


def write_pcm16_wav(path: Path, pcm: bytes) -> None:
    validate_pcm16(pcm)
    with wave.open(str(path), "wb") as wav:
        wav.setnchannels(CHANNELS)
        wav.setsampwidth(SAMPLE_WIDTH)
        wav.setframerate(SAMPLE_RATE)
        wav.writeframes(pcm)


def read_health_wav(path: Path) -> bytes:
    try:
        with wave.open(str(path), "rb") as wav:
            channels = wav.getnchannels()
            width = wav.getsampwidth()
            rate = wav.getframerate()
            frames = wav.readframes(wav.getnframes())
    except (OSError, wave.Error) as exc:
        raise AudioError(f"health audio is not a readable WAV: {exc}") from exc
    if channels != CHANNELS or width != SAMPLE_WIDTH or rate != SAMPLE_RATE:
        raise AudioError(
            "health audio must be mono PCM16 at 16000 Hz "
            f"(got channels={channels}, width={width}, rate={rate})"
        )
    validate_pcm16(frames)
    return frames


_TOKEN_RE = re.compile(r"[^\W_]+(?:['’][^\W_]+)?", re.UNICODE)


def normalize_tokens(text: str) -> tuple[str, ...]:
    """Normalize text to an exact token sequence for the health gate.

    This deliberately does not use substring or character membership checks.
    Punctuation and case are ignored, while token order and count remain exact.
    """

    if not isinstance(text, str):
        raise AudioError("transcript must be text")
    return tuple(token.casefold() for token in _TOKEN_RE.findall(text))


def health_phrase_matches(actual: str, expected: str) -> bool:
    expected_tokens = normalize_tokens(expected)
    actual_tokens = normalize_tokens(actual)
    return bool(expected_tokens) and actual_tokens == expected_tokens
