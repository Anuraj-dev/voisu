"""Bounded length-prefixed worker transport."""

from __future__ import annotations

import json
import re
from typing import BinaryIO, Any


MAX_JSON_BYTES = 64 * 1024
MAX_PCM_BYTES = 19_200_000
MAX_ID_BYTES = 128


class ProtocolError(ValueError):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code
        self.message = message


def _read_exact(stream: BinaryIO, size: int) -> bytes | None:
    chunks: list[bytes] = []
    remaining = size
    while remaining:
        chunk = stream.read(remaining)
        if not chunk:
            if not chunks:
                return None
            raise ProtocolError("eof_mid_frame", "input ended in a frame")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_json_frame(stream: BinaryIO) -> dict[str, Any] | None:
    header = _read_exact(stream, 4)
    if header is None:
        return None
    size = int.from_bytes(header, "little")
    if size <= 0 or size > MAX_JSON_BYTES:
        raise ProtocolError("frame_too_large", "JSON frame exceeds the 64 KiB limit")
    payload = _read_exact(stream, size)
    if payload is None:
        raise ProtocolError("eof_mid_frame", "input ended before JSON frame")
    try:
        value = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ProtocolError("invalid_json", "request frame is not valid UTF-8 JSON") from exc
    if not isinstance(value, dict):
        raise ProtocolError("invalid_request", "request frame must be a JSON object")
    return value


def read_pcm(stream: BinaryIO, size: int) -> bytes:
    if not isinstance(size, int) or isinstance(size, bool):
        raise ProtocolError("invalid_pcm_bytes", "pcm_bytes must be an integer")
    if size <= 0 or size > MAX_PCM_BYTES or size % 2:
        raise ProtocolError("invalid_pcm_bytes", "PCM size is outside the bounded mono PCM16 range")
    payload = _read_exact(stream, size)
    if payload is None:
        raise ProtocolError("eof_mid_pcm", "input ended before PCM payload")
    return payload


def _bounded_id(value: Any) -> str:
    if not isinstance(value, str) or not value or len(value.encode("utf-8")) > MAX_ID_BYTES:
        raise ProtocolError("invalid_id", "request id must be a non-empty bounded string")
    return value


def request_id(request: dict[str, Any]) -> str:
    return _bounded_id(request.get("id"))


def write_json_frame(stream: BinaryIO, value: dict[str, Any]) -> None:
    try:
        payload = json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode("utf-8")
    except (TypeError, ValueError) as exc:
        raise ProtocolError("response_encoding", "response could not be encoded") from exc
    if len(payload) > MAX_JSON_BYTES:
        raise ProtocolError("response_too_large", "response exceeds the 64 KiB limit")
    stream.write(len(payload).to_bytes(4, "little"))
    stream.write(payload)
    stream.flush()


_CONTROL_RE = re.compile(r"[\x00-\x1f\x7f]")


def sanitize_message(message: object, limit: int = 320) -> str:
    text = _CONTROL_RE.sub(" ", str(message)).strip()
    text = " ".join(text.split())
    return text[:limit] or "runtime error"
