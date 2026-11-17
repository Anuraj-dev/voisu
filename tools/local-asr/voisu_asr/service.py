"""Worker lifecycle and framed protocol service."""

from __future__ import annotations

import contextlib
import os
import re
import sys
import time
import unicodedata
from pathlib import Path
from typing import BinaryIO, Callable

from .adapters import AdapterError, BaseAdapter, create_adapter
from .audio import AudioError, health_phrase_matches, read_health_wav, validate_pcm16
from .manifest import Manifest, ManifestError, load_manifest
from .protocol import ProtocolError, read_json_frame, read_pcm, request_id, sanitize_message, write_json_frame
from .windowing import WindowingError, transcribe_windows

WORKER_ENTRY = Path(__file__).resolve().parents[1] / "worker.py"


def _error_response(request_id_value: str, code: str, message: object) -> dict[str, object]:
    return {
        "v": 1,
        "id": request_id_value,
        "status": "error",
        "code": code,
        "message": sanitize_message(message),
    }


def _ready_response(manifest: Manifest, adapter: BaseAdapter, request_id_value: str) -> dict[str, object]:
    metadata = adapter.metadata
    response: dict[str, object] = {
        "v": 1,
        "id": request_id_value,
        "status": "ready",
        "model_id": manifest.model_id,
        "observed_device": metadata.observed_device,
        "resident": metadata.resident,
        "backend": metadata.backend,
        "peak_host_ram_bytes": metadata.peak_host_ram_bytes,
        "peak_gpu_memory_bytes": metadata.peak_gpu_memory_bytes,
    }
    # Capability negotiation: the daemon sends a transcribe `prompt` only to a
    # worker that said so here. An older worker never does, and then none is sent.
    if adapter.accepts_initial_prompt:
        response["accepts_prompt"] = True
    return response


# Mirrors the daemon's MAX_PROMPT_BYTES. The daemon builds the prompt within a
# far smaller Whisper token budget; this only bounds a misbehaving peer.
MAX_PROMPT_BYTES = 1024


def _validated_prompt(request: dict) -> str | None:
    """The optional `prompt` field, under the one rule the daemon also applies:
    non-empty after trimming, within MAX_PROMPT_BYTES, no control characters."""

    if "prompt" not in request:
        return None
    prompt = request["prompt"]
    if (
        not isinstance(prompt, str)
        or not prompt.strip()
        or len(prompt.encode("utf-8")) > MAX_PROMPT_BYTES
        or any(unicodedata.category(ch) == "Cc" for ch in prompt)
    ):
        raise ProtocolError("invalid_request", "prompt must be a non-empty string without control characters within the size limit")
    return prompt


def _is_prompt_echo(text: str, prompt: str) -> bool:
    """True when `text` only recites the prompt glossary.

    A quiet window can make Whisper emit its own prompt. That is two or more
    distinct prompt terms joined by commas or spaces, optionally ending in a
    period, and optionally led by the daemon's `Glossary:` sentence opener.
    A single term stays: people do dictate one name.
    """

    glossary = re.fullmatch(r"\s*Glossary:\s*(.*?)\.?\s*", prompt, re.DOTALL)
    listed = glossary.group(1) if glossary else prompt
    terms = sorted({term.strip() for term in listed.split(",") if term.strip()}, key=len, reverse=True)
    if len(terms) < 2:
        return False
    term = "|".join(re.escape(item) for item in terms)
    recital = re.fullmatch(
        rf"\s*(?:glossary:?\s*)?((?:{term})(?:[\s,]+(?:{term}))+)[\s,]*\.?\s*", text, re.IGNORECASE
    )
    if recital is None:
        return False
    return len({found.lower() for found in re.findall(term, recital.group(1), re.IGNORECASE)}) >= 2


def _transcribe(
    adapter: BaseAdapter, pcm: bytes, prompt: str | None = None
) -> tuple[str, dict[str, int]]:
    """Transcribe one Recording window by window; return text and request peaks.

    The adapter sees at most one bounded window per call, so its peak memory
    no longer grows with Recording length. Adapters report peaks for their
    latest call; the request peak is the maximum over the windows it ran.
    A GPU peak of 0 means "unknown", so if any window that ran reported 0, or
    no window ran (all-zero PCM), the request's GPU peak is 0 too: a max over
    the other windows would understate the real peak. Host RAM is a
    process-lifetime high-water mark, so it is never stale.
    """

    host_peak = adapter.metadata.peak_host_ram_bytes
    gpu_peaks: list[int] = []

    def transcribe_window(window: bytes) -> str:
        nonlocal host_peak
        # Only an adapter that declares prompt support is handed one; every
        # other adapter keeps its one-argument transcribe(pcm).
        if prompt is not None and adapter.accepts_initial_prompt:
            text = adapter.transcribe(window, initial_prompt=prompt)
            if _is_prompt_echo(text, prompt):
                # Local diagnostic only, and it never names the terms: lets the
                # bench and real use show whether this ever eats real speech.
                print("voisu-asr: dropped a window that only listed Dictionary terms", file=sys.stderr)
                text = ""
        else:
            text = adapter.transcribe(window)
        metadata = adapter.metadata
        host_peak = max(host_peak, metadata.peak_host_ram_bytes)
        gpu_peaks.append(metadata.peak_gpu_memory_bytes)
        return text

    text = transcribe_windows(pcm, transcribe_window)
    gpu_peak = 0 if not gpu_peaks or 0 in gpu_peaks else max(gpu_peaks)
    return text, {"peak_host_ram_bytes": host_peak, "peak_gpu_memory_bytes": gpu_peak}


def _cpu_seconds() -> float:
    """Worker user+system CPU time, including reaped child runtimes."""

    times = os.times()
    return times.user + times.system + times.children_user + times.children_system


def _prepare(manifest: Manifest, adapter: BaseAdapter) -> None:
    """Run one real health inference before allowing any request."""

    health_audio = read_health_wav(Path(manifest.options["health_audio"]))
    validate_pcm16(health_audio)
    health_text, _ = _transcribe(adapter, health_audio)
    if not health_text.strip():
        raise AdapterError("health_empty", "health inference returned no text")
    if not health_phrase_matches(health_text, manifest.options["health_expected"]):
        raise AdapterError("health_mismatch", "health inference did not match the pinned phrase")


def serve_stream(
    manifest: Manifest,
    adapter: BaseAdapter,
    reader: BinaryIO,
    writer: BinaryIO,
) -> None:
    """Serve a validated manifest over pipes; useful for contract tests."""

    prepared = False
    seen_ids: set[str] = set()
    try:
        while True:
            request = read_json_frame(reader)
            if request is None:
                return
            try:
                request_id_value = request_id(request)
                if request_id_value in seen_ids:
                    raise ProtocolError("duplicate_id", "request id must be unique for this worker")
                seen_ids.add(request_id_value)
                if request.get("v") != 1:
                    raise ProtocolError("unsupported_version", "request protocol version must be 1")
                operation = request.get("op")
                if operation == "prepare":
                    if set(request) != {"v", "op", "id"}:
                        raise ProtocolError("invalid_request", "prepare has unknown fields")
                    if not prepared:
                        with contextlib.redirect_stdout(sys.stderr):
                            _prepare(manifest, adapter)
                        prepared = True
                    write_json_frame(writer, _ready_response(manifest, adapter, request_id_value))
                elif operation == "transcribe":
                    if "pcm_bytes" not in request:
                        raise ProtocolError("invalid_request", "transcribe needs pcm_bytes")
                    # Read the PCM before judging anything else, so a rejected
                    # request (unknown key, bad prompt) never leaves its audio
                    # in the stream to be parsed as the next frame header.
                    pcm = read_pcm(reader, request["pcm_bytes"])
                    if not {"v", "op", "id", "pcm_bytes"} <= set(request) <= {"v", "op", "id", "pcm_bytes", "prompt"}:
                        raise ProtocolError("invalid_request", "transcribe has unknown fields")
                    prompt = _validated_prompt(request)
                    if not prepared:
                        write_json_frame(writer, _error_response(request_id_value, "not_ready", "send prepare before transcribe"))
                        continue
                    started = time.monotonic_ns()
                    cpu_started = _cpu_seconds()
                    with contextlib.redirect_stdout(sys.stderr):
                        text, peaks = _transcribe(adapter, pcm, prompt)
                    elapsed_ms = max(0, (time.monotonic_ns() - started) // 1_000_000)
                    peaks["cpu_time_ms"] = max(0, round((_cpu_seconds() - cpu_started) * 1000))
                    if text.strip():
                        write_json_frame(
                            writer,
                            {
                                "v": 1,
                                "id": request_id_value,
                                "status": "transcript",
                                "text": text.strip(),
                                "observed_device": adapter.metadata.observed_device,
                                "resident": adapter.metadata.resident,
                                "inference_ms": elapsed_ms,
                                **peaks,
                            },
                        )
                    else:
                        write_json_frame(
                            writer,
                            {
                                "v": 1,
                                "id": request_id_value,
                                "status": "no_text",
                                "observed_device": adapter.metadata.observed_device,
                                "resident": adapter.metadata.resident,
                                "inference_ms": elapsed_ms,
                                **peaks,
                            },
                        )
                elif operation == "shutdown":
                    if set(request) != {"v", "op", "id"}:
                        raise ProtocolError("invalid_request", "shutdown has unknown fields")
                    write_json_frame(writer, {"v": 1, "id": request_id_value, "status": "shutdown"})
                    return
                else:
                    raise ProtocolError("unknown_operation", "operation must be prepare, transcribe, or shutdown")
            except (AdapterError, AudioError, ProtocolError, WindowingError) as exc:
                code = getattr(exc, "code", "worker_error")
                message = getattr(exc, "message", str(exc))
                write_json_frame(writer, _error_response(locals().get("request_id_value", ""), code, message))
    finally:
        with contextlib.redirect_stdout(sys.stderr):
            adapter.close()


def run(manifest_path: str | Path, *, stdin: BinaryIO | None = None, stdout: BinaryIO | None = None) -> int:
    """Load, validate, and serve a private worker manifest."""

    input_stream = sys.stdin.buffer if stdin is None else stdin
    output_stream = sys.stdout.buffer if stdout is None else stdout
    try:
        manifest = load_manifest(manifest_path, expected_worker=WORKER_ENTRY)
        with contextlib.redirect_stdout(sys.stderr):
            adapter = create_adapter(manifest)
    except (ManifestError, AdapterError) as exc:
        write_json_frame(output_stream, _error_response("", getattr(exc, "code", "manifest_error"), getattr(exc, "message", str(exc))))
        return 2
    serve_stream(manifest, adapter, input_stream, output_stream)
    return 0
