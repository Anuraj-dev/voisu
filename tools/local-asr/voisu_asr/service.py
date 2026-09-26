"""Worker lifecycle and framed protocol service."""

from __future__ import annotations

import contextlib
import sys
import time
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
    return {
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


def _transcribe(adapter: BaseAdapter, pcm: bytes) -> tuple[str, dict[str, int]]:
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
        text = adapter.transcribe(window)
        metadata = adapter.metadata
        host_peak = max(host_peak, metadata.peak_host_ram_bytes)
        gpu_peaks.append(metadata.peak_gpu_memory_bytes)
        return text

    text = transcribe_windows(pcm, transcribe_window)
    gpu_peak = 0 if not gpu_peaks or 0 in gpu_peaks else max(gpu_peaks)
    return text, {"peak_host_ram_bytes": host_peak, "peak_gpu_memory_bytes": gpu_peak}


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
                    if set(request) != {"v", "op", "id", "pcm_bytes"}:
                        raise ProtocolError("invalid_request", "transcribe has unknown fields")
                    pcm = read_pcm(reader, request["pcm_bytes"])
                    if not prepared:
                        write_json_frame(writer, _error_response(request_id_value, "not_ready", "send prepare before transcribe"))
                        continue
                    started = time.monotonic_ns()
                    with contextlib.redirect_stdout(sys.stderr):
                        text, peaks = _transcribe(adapter, pcm)
                    elapsed_ms = max(0, (time.monotonic_ns() - started) // 1_000_000)
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
