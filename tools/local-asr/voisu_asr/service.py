"""Worker lifecycle and framed protocol service."""

from __future__ import annotations

import contextlib
import os
import sys
import time
from pathlib import Path
from typing import BinaryIO, Callable

from .adapters import AdapterError, BaseAdapter, create_adapter
from .audio import AudioError, health_phrase_matches, read_health_wav, validate_pcm16
from .manifest import Manifest, ManifestError, load_manifest
from .protocol import ProtocolError, read_json_frame, read_pcm, request_id, sanitize_message, write_json_frame

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


def _cpu_seconds() -> float:
    """Worker user+system CPU time, including reaped child runtimes."""

    times = os.times()
    return times.user + times.system + times.children_user + times.children_system


def _prepare(manifest: Manifest, adapter: BaseAdapter) -> None:
    """Run one real health inference before allowing any request."""

    health_audio = read_health_wav(Path(manifest.options["health_audio"]))
    validate_pcm16(health_audio)
    health_text = adapter.transcribe(health_audio)
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
                    cpu_started = _cpu_seconds()
                    with contextlib.redirect_stdout(sys.stderr):
                        text = adapter.transcribe(pcm)
                    elapsed_ms = max(0, (time.monotonic_ns() - started) // 1_000_000)
                    peaks = {
                        "peak_host_ram_bytes": adapter.metadata.peak_host_ram_bytes,
                        "peak_gpu_memory_bytes": adapter.metadata.peak_gpu_memory_bytes,
                        "cpu_time_ms": max(0, round((_cpu_seconds() - cpu_started) * 1000)),
                    }
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
            except (AdapterError, AudioError, ProtocolError) as exc:
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
