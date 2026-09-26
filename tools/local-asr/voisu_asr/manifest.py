"""Private development manifest validation and artifact pin checks."""

from __future__ import annotations

import hashlib
import json
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .candidates import CANDIDATE_OPTIONS, CANDIDATES, candidate_error


MODEL_IDS = frozenset(
    {
        "nemotron-3.5-0.6b",
        "parakeet-tdt-0.6b-v3",
        "qwen3-asr-0.6b",
        "qwen3-asr-1.7b",
        "fun-asr-nano-2512",
        "moonshine-streaming-medium",
        "cohere-transcribe-03-2026",
        *CANDIDATES,
    }
)
DEVICES = frozenset({"cpu", "cuda:0"})
LANGUAGES = frozenset({"en", "hi", "auto"})
_SHA256 = re.compile(r"^[0-9a-f]{64}$")

_TOP_LEVEL = frozenset(
    {"version", "model_id", "device", "language", "runtime", "model_path", "files", "options", "limits"}
)
_RUNTIME_KEYS = frozenset({"python", "worker", "revision", "requirements_lock"})
_LIMIT_KEYS = frozenset({"memory_bytes", "tasks", "cpu_cores", "cache_bytes", "vram_bytes"})
_COMMON_OPTIONS = frozenset({"health_audio", "health_expected", "cache_dir"})
_MODEL_OPTIONS = {
    "nemotron-3.5-0.6b": frozenset({"library", "language_code"}),
    "parakeet-tdt-0.6b-v3": frozenset({"library", "language_code"}),
    "qwen3-asr-0.6b": frozenset({"revision", "dtype", "max_new_tokens"}),
    "qwen3-asr-1.7b": frozenset({"revision", "dtype", "max_new_tokens"}),
    "fun-asr-nano-2512": frozenset(
        {
            "hub",
            "remote_code",
            "itn",
            "checkpoint",
            "encoder",
            "decoder",
            "vad",
            "llama_cli",
            "revision",
            "dtype",
            "max_new_tokens",
        }
    ),
    "moonshine-streaming-medium": frozenset({"model_arch"}),
    "cohere-transcribe-03-2026": frozenset({"revision", "punctuation", "max_new_tokens"}),
    **{model_id: CANDIDATE_OPTIONS[item.backend] for model_id, item in CANDIDATES.items()},
}
FUN_ASR_NANO_CHECKPOINT = "Fun-ASR-Nano-2512"
FUN_ASR_MLT_MARKER = "fun-asr-mlt-nano"
FUN_ASR_ENCODER_NAME = "funasr-encoder-f16.gguf"
FUN_ASR_ENCODER_SHA = "f92f91d01a24fbed6c863495b2ee8c6a6788144a02858b75743f0946668de8a2"
FUN_ASR_ENCODER_BYTES = 469_331_008
FUN_ASR_DECODER_Q8_NAME = "qwen3-0.6b-q8_0.gguf"
FUN_ASR_DECODER_Q8_SHA = "819f385dc0e035dccc3d9e7edaf6b7b044b8ba7ace63cbcbf84c7e397eecbf27"
FUN_ASR_DECODER_Q8_BYTES = 804_753_280
FUN_ASR_VAD_NAME = "fsmn-vad.gguf"
FUN_ASR_VAD_SHA = "1270f2559c495f4e7b6e739541151027d360761a3fda43fc147034f5719f5479"
FUN_ASR_VAD_BYTES = 1_720_512
FUN_ASR_LLAMA_RUNTIME = "runtime-llamacpp-v0.2.6"
FUN_ASR_PYTHON_ENGLISH = "英文"
FUN_ASR_HF_SOURCE = "https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-2512-hf"
FUN_ASR_HF_REV = "d93b302ee7fd505e1b3576120fc142fc6f7820e1"
FUN_ASR_HF_WEIGHTS_NAME = "model.safetensors"
FUN_ASR_HF_WEIGHTS_SHA = "1bbb6dcc5d8b75084a399d48c4d4b0f3aa1d3f09f2616ae40ed2c4fba03d89c9"
FUN_ASR_HF_WEIGHTS_BYTES = 1_659_773_320
QWEN_REQUIRED_FILES = (
    "config.json",
    "generation_config.json",
    "model.safetensors",
    "processor_config.json",
    "tokenizer.json",
    "tokenizer_config.json",
)
MOONSHINE_MEDIUM_ARCH = "MEDIUM_STREAMING"
MOONSHINE_VOICE_SOURCE = "https://download.moonshine.ai/model/medium-streaming-en/quantized"
MOONSHINE_MEDIUM_FILES = (
    "encoder.ort",
    "adapter.ort",
    "cross_kv.ort",
    "decoder_kv.ort",
    "frontend.ort",
    "streaming_config.json",
    "tokenizer.bin",
)
MOONSHINE_MEDIUM_PINS = {
    "encoder.ort": ("a5f11167a62eef61787fe8410453257d6ddb8eba90af461a9604e5f2e93d5322", 94_202_872),
    "adapter.ort": ("16307442b7f4229f2f1511fc51b545cec9616e55872c588f3a297bbc6f4762ea", 3_647_712),
    "cross_kv.ort": ("354b9a955caeb768b528f447f0a36ce4b850ca7b4531900165df304d97904fba", 11_544_952),
    "decoder_kv.ort": ("fa67aa87521247f5bf44d3e44d4e4978e58c1f114249c3c6909c882624056715", 146_216_448),
    "frontend.ort": ("378fe8a5d7090a1b9ab88bbb1fc95bde010cdd64ec23419350d2d23c675636e9", 47_467_256),
    "streaming_config.json": ("28e83b7a28e91472692a035e0dae3116422ae43aeb2bef5ed822c44ce89b88af", 513),
    "tokenizer.bin": ("6884b35fd6377d4c4d32336a0bc152f36b64d1e45b6503683cdc238250a8472d", 249_974),
}
COHERE_HF_REV = "b1eacc2686a3d08ceaae5f24a88b1d519620bc09"
COHERE_HF_SOURCE = "https://huggingface.co/CohereLabs/cohere-transcribe-03-2026"
_GIT_SHA = re.compile(r"^[0-9a-f]{40}$")


class ManifestError(ValueError):
    """A manifest cannot safely identify a local runtime and its artifacts."""


@dataclass(frozen=True)
class Artifact:
    path: Path
    sha256: str
    bytes: int


@dataclass(frozen=True)
class Manifest:
    version: int
    model_id: str
    device: str
    language: str
    python: Path
    worker: Path
    revision: str
    requirements_lock: Path | None
    model_path: Path
    artifacts: tuple[Artifact, ...]
    options: dict[str, Any]
    limits: dict[str, int]
    source_path: Path


def _require_keys(obj: dict[str, Any], allowed: frozenset[str], label: str) -> None:
    unknown = set(obj) - allowed
    if unknown:
        raise ManifestError(f"{label} contains unknown setting(s): {', '.join(sorted(unknown))}")


def _absolute_path(value: Any, label: str) -> Path:
    if not isinstance(value, str) or not value:
        raise ManifestError(f"{label} must be an absolute path")
    path = Path(value)
    if not path.is_absolute():
        raise ManifestError(f"{label} must be an absolute path")
    return path


def _hash_file(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    try:
        with path.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                digest.update(chunk)
                size += len(chunk)
    except OSError as exc:
        raise ManifestError(f"cannot read pinned artifact {path}: {exc}") from exc
    return digest.hexdigest(), size


def _artifact(value: Any, index: int) -> Artifact:
    if not isinstance(value, dict) or set(value) != {"path", "sha256", "bytes"}:
        raise ManifestError(f"files[{index}] must contain path, sha256, and bytes")
    path = _absolute_path(value["path"], f"files[{index}].path")
    sha256 = value["sha256"]
    size = value["bytes"]
    if not isinstance(sha256, str) or not _SHA256.fullmatch(sha256):
        raise ManifestError(f"files[{index}].sha256 must be a lowercase SHA-256 digest")
    if not isinstance(size, int) or isinstance(size, bool) or size < 0:
        raise ManifestError(f"files[{index}].bytes must be a non-negative integer")
    if not path.is_file():
        raise ManifestError(f"pinned artifact is not a regular file: {path}")
    actual_hash, actual_size = _hash_file(path)
    if actual_hash != sha256 or actual_size != size:
        raise ManifestError(f"pinned artifact changed: {path}")
    return Artifact(path=path, sha256=sha256, bytes=size)


def _contains_mlt(value: str) -> bool:
    return FUN_ASR_MLT_MARKER in value.lower()


def _validate_capabilities(model_id: str, device: str, language: str) -> None:
    if model_id in {"moonshine-streaming-medium"} and device != "cpu":
        raise ManifestError("Moonshine Medium is currently supported only on its CPU runtime")
    if model_id in {"parakeet-tdt-0.6b-v3", "fun-asr-nano-2512", "moonshine-streaming-medium", "cohere-transcribe-03-2026"} and language == "hi":
        raise ManifestError(f"{model_id} has no verified Hindi capability")
    if model_id in {"parakeet-tdt-0.6b-v3", "fun-asr-nano-2512", "moonshine-streaming-medium", "cohere-transcribe-03-2026"} and language == "auto":
        raise ManifestError(f"{model_id} has no verified automatic language selection")


def _pinned_option_file(options: dict[str, Any], key: str, artifact_paths: set[Path]) -> Path:
    path = _absolute_path(options.get(key), f"options.{key}")
    if not path.is_file() or path not in artifact_paths:
        raise ManifestError(f"{key} must be a pinned regular file")
    return path


def _require_published_artifact(artifacts: tuple[Artifact, ...], path: Path, sha: str, size: int, label: str) -> None:
    for item in artifacts:
        if item.path != path:
            continue
        if item.sha256 != sha or item.bytes != size:
            raise ManifestError(f"{label} does not match the published artifact")
        return
    raise ManifestError(f"{label} must be a pinned regular file")


def moonshine_medium_root(model_path: Path | str) -> Path:
    root = Path(model_path)
    if not root.is_dir():
        raise ManifestError("moonshine-streaming-medium model_path must be a directory")
    return root


def moonshine_medium_paths(model_path: Path | str) -> tuple[Path, ...]:
    root = moonshine_medium_root(model_path)
    return tuple(root / name for name in MOONSHINE_MEDIUM_FILES)


def verify_moonshine_medium_pins(model_path: Path | str) -> None:
    """Require all seven official Medium Streaming files at published digest/size.

    Dummy .ort stubs fail here. Existence is not identity. The worker path must
    call this even when a Manifest object skipped load_manifest.
    """

    for path in moonshine_medium_paths(model_path):
        name = path.name
        if not path.is_file():
            raise ManifestError(
                "moonshine-streaming-medium requires the official moonshine-voice Medium Streaming .ort bundle"
            )
        sha, size = MOONSHINE_MEDIUM_PINS[name]
        try:
            actual_size = path.stat().st_size
        except OSError as exc:
            raise ManifestError(f"cannot read pinned artifact {path}: {exc}") from exc
        if actual_size != size:
            raise ManifestError(f"{name} does not match the published artifact")
        actual_hash, hashed_size = _hash_file(path)
        if actual_hash != sha or hashed_size != size:
            raise ManifestError(f"{name} does not match the published artifact")


def _validate_remaining_model_options(
    model_id: str,
    model_path: Path,
    options: dict[str, Any],
    artifacts: tuple[Artifact, ...],
) -> None:
    artifact_paths = {item.path for item in artifacts}
    if model_id == "fun-asr-nano-2512":
        if _contains_mlt(str(model_path)) or any(_contains_mlt(str(value)) for value in options.values()):
            raise ManifestError("Fun-ASR-MLT-Nano is not a substitute for Fun-ASR-Nano-2512")
        if options.get("checkpoint") != FUN_ASR_NANO_CHECKPOINT:
            raise ManifestError("fun-asr-nano-2512 requires checkpoint Fun-ASR-Nano-2512")
        has_native = all(key in options for key in ("encoder", "decoder", "llama_cli"))
        has_python = "remote_code" in options
        has_transformers = options.get("revision") == FUN_ASR_HF_REV
        if not has_native and not has_python and not has_transformers:
            raise ManifestError(
                "fun-asr-nano-2512 requires the llama.cpp encoder/decoder/llama_cli bundle, pinned remote_code, or Fun-ASR-Nano-2512-hf revision"
            )
        if has_native:
            encoder = _pinned_option_file(options, "encoder", artifact_paths)
            decoder = _pinned_option_file(options, "decoder", artifact_paths)
            _pinned_option_file(options, "llama_cli", artifact_paths)
            if encoder.name != FUN_ASR_ENCODER_NAME:
                raise ManifestError("fun-asr-nano-2512 encoder must be funasr-encoder-f16.gguf")
            if decoder.name != FUN_ASR_DECODER_Q8_NAME:
                raise ManifestError("fun-asr-nano-2512 decoder must be qwen3-0.6b-q8_0.gguf")
            _require_published_artifact(
                artifacts, encoder, FUN_ASR_ENCODER_SHA, FUN_ASR_ENCODER_BYTES, "Fun-ASR-Nano encoder"
            )
            _require_published_artifact(
                artifacts, decoder, FUN_ASR_DECODER_Q8_SHA, FUN_ASR_DECODER_Q8_BYTES, "Fun-ASR-Nano decoder"
            )
            if "vad" in options:
                vad = _pinned_option_file(options, "vad", artifact_paths)
                if vad.name != FUN_ASR_VAD_NAME:
                    raise ManifestError("fun-asr-nano-2512 vad must be fsmn-vad.gguf")
                _require_published_artifact(
                    artifacts, vad, FUN_ASR_VAD_SHA, FUN_ASR_VAD_BYTES, "Fun-ASR-Nano vad"
                )
        if has_python:
            _pinned_option_file(options, "remote_code", artifact_paths)
        revision = options.get("revision")
        if revision is not None and revision != FUN_ASR_HF_REV:
            raise ManifestError("fun-asr-nano-2512 Transformers revision must be the pinned Fun-ASR-Nano-2512-hf commit")
        root = model_path if model_path.is_dir() else model_path.parent
        weights = root / FUN_ASR_HF_WEIGHTS_NAME
        if has_transformers:
            if weights not in artifact_paths or not weights.is_file():
                raise ManifestError(
                    "fun-asr-nano-2512 Transformers path requires pinned Fun-ASR-Nano-2512-hf weights"
                )
            _require_published_artifact(
                artifacts, weights, FUN_ASR_HF_WEIGHTS_SHA, FUN_ASR_HF_WEIGHTS_BYTES, "Fun-ASR-Nano-2512-hf weights"
            )
        elif weights in artifact_paths:
            _require_published_artifact(
                artifacts, weights, FUN_ASR_HF_WEIGHTS_SHA, FUN_ASR_HF_WEIGHTS_BYTES, "Fun-ASR-Nano-2512-hf weights"
            )
    elif model_id in {"qwen3-asr-0.6b", "qwen3-asr-1.7b"}:
        if not model_path.is_dir():
            raise ManifestError("Qwen model_path must be a directory of pinned Transformers files")
        for name in QWEN_REQUIRED_FILES:
            path = model_path / name
            if path not in artifact_paths:
                raise ManifestError(f"Qwen Transformers file is not pinned: {name}")
    elif model_id == "moonshine-streaming-medium":
        if options.get("model_arch") != MOONSHINE_MEDIUM_ARCH:
            raise ManifestError("moonshine-streaming-medium requires model_arch=MEDIUM_STREAMING")
        root = moonshine_medium_root(model_path)
        for name in MOONSHINE_MEDIUM_FILES:
            path = root / name
            if path not in artifact_paths or not path.is_file():
                raise ManifestError(
                    "moonshine-streaming-medium requires the official moonshine-voice Medium Streaming .ort bundle"
                )
            sha, size = MOONSHINE_MEDIUM_PINS[name]
            _require_published_artifact(artifacts, path, sha, size, name)
    elif model_id == "cohere-transcribe-03-2026":
        revision = options.get("revision")
        if not isinstance(revision, str) or not _GIT_SHA.fullmatch(revision):
            raise ManifestError("cohere-transcribe-03-2026 requires a pinned 40-hex git revision")
        if revision != COHERE_HF_REV:
            raise ManifestError("cohere-transcribe-03-2026 revision must be b1eacc2686a3d08ceaae5f24a88b1d519620bc09")


def load_manifest(path: str | Path, *, expected_worker: Path | None = None) -> Manifest:
    source_path = Path(path).absolute()
    try:
        raw = source_path.read_bytes()
        document = json.loads(raw.decode("utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ManifestError(f"cannot read manifest: {exc}") from exc
    if not isinstance(document, dict):
        raise ManifestError("manifest must be a JSON object")
    _require_keys(document, _TOP_LEVEL, "manifest")
    if document.get("version") != 1:
        raise ManifestError("manifest version must be 1")
    model_id = document.get("model_id")
    if model_id not in MODEL_IDS:
        raise ManifestError("manifest model_id is not a development catalog model")
    device = document.get("device")
    language = document.get("language")
    if device not in DEVICES:
        raise ManifestError("manifest device must be cpu or cuda:0")
    if language not in LANGUAGES:
        raise ManifestError("manifest language must be en, hi, or auto")
    if device == "cpu" and document.get("limits", {}).get("vram_bytes", 0) != 0:
        raise ManifestError("vram_bytes must be zero for CPU manifests")
    if device.startswith("cuda:") and document.get("limits", {}).get("vram_bytes", 0) <= 0:
        raise ManifestError("CUDA manifests require a positive vram_bytes admission limit")
    _validate_capabilities(model_id, device, language)

    runtime = document.get("runtime")
    if not isinstance(runtime, dict):
        raise ManifestError("runtime must be an object")
    _require_keys(runtime, _RUNTIME_KEYS, "runtime")
    python = _absolute_path(runtime.get("python"), "runtime.python")
    worker = _absolute_path(runtime.get("worker"), "runtime.worker")
    revision = runtime.get("revision")
    if not isinstance(revision, str) or not revision.strip():
        raise ManifestError("runtime.revision must identify a pinned build")
    if not python.is_file() or not os.access(python, os.X_OK):
        raise ManifestError(f"runtime.python is not executable: {python}")
    if not worker.is_file():
        raise ManifestError(f"runtime.worker is not a regular file: {worker}")
    if expected_worker is not None and worker != expected_worker.absolute():
        raise ManifestError("manifest worker does not match the running worker")
    lock = runtime.get("requirements_lock")
    requirements_lock = None if lock is None else _absolute_path(lock, "runtime.requirements_lock")
    if requirements_lock is not None and not requirements_lock.is_file():
        raise ManifestError(f"runtime.requirements_lock is not a regular file: {requirements_lock}")

    model_path = _absolute_path(document.get("model_path"), "model_path")
    if not model_path.exists():
        raise ManifestError(f"model_path does not exist: {model_path}")
    files = document.get("files")
    if not isinstance(files, list) or not files:
        raise ManifestError("files must be a non-empty artifact list")
    artifacts = tuple(_artifact(value, i) for i, value in enumerate(files))
    artifact_paths = {item.path for item in artifacts}
    for required in (python, worker):
        if required not in artifact_paths:
            raise ManifestError(f"runtime file is not pinned in files: {required}")
    if requirements_lock is not None and requirements_lock not in artifact_paths:
        raise ManifestError("runtime.requirements_lock is not pinned in files")
    if model_path.is_file() and model_path not in artifact_paths:
        raise ManifestError("model_path file is not pinned in files")

    options = document.get("options")
    if not isinstance(options, dict):
        raise ManifestError("options must be an object")
    allowed_options = _COMMON_OPTIONS | _MODEL_OPTIONS[model_id]
    unknown_options = set(options) - allowed_options
    if unknown_options:
        raise ManifestError(f"options contains unknown setting(s): {', '.join(sorted(unknown_options))}")
    if "cache_dir" in options:
        _absolute_path(options.get("cache_dir"), "options.cache_dir")
    health_audio = options.get("health_audio")
    expected = options.get("health_expected")
    health_path = _absolute_path(health_audio, "options.health_audio")
    if not health_path.is_file():
        raise ManifestError("options.health_audio is not a regular file")
    if health_path not in artifact_paths:
        raise ManifestError("options.health_audio is not pinned in files")
    if not isinstance(expected, str) or not expected.strip():
        raise ManifestError("options.health_expected must be a non-empty phrase")

    if model_id in {"nemotron-3.5-0.6b", "parakeet-tdt-0.6b-v3"}:
        library = _absolute_path(options.get("library"), "options.library")
        if not library.is_file() or library not in artifact_paths:
            raise ManifestError("native NeMo model requires a pinned options.library file")
    if model_id == "parakeet-tdt-0.6b-v3":
        code = options.get("language_code")
        if code is not None and code not in {"en", "en-US"}:
            raise ManifestError("parakeet-tdt-0.6b-v3 language_code must be en or en-US")
    _validate_remaining_model_options(model_id, model_path, options, artifacts)
    limits = document.get("limits")
    if not isinstance(limits, dict):
        raise ManifestError("limits must be an object")
    _require_keys(limits, _LIMIT_KEYS, "limits")
    normalized_limits: dict[str, int] = {}
    for name in _LIMIT_KEYS:
        value = limits.get(name)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            raise ManifestError(f"limits.{name} must be a non-negative integer")
        normalized_limits[name] = value
    if normalized_limits["tasks"] <= 0 or normalized_limits["cpu_cores"] <= 0 or normalized_limits["memory_bytes"] <= 0:
        raise ManifestError("memory_bytes, tasks, and cpu_cores must be positive")
    if model_id in CANDIDATES:
        pins = ((item.path, item.sha256, item.bytes) for item in artifacts)
        error = candidate_error(
            CANDIDATES[model_id], device, language, model_path, options, normalized_limits["vram_bytes"], pins
        )
        if error:
            raise ManifestError(error)

    return Manifest(
        version=1,
        model_id=model_id,
        device=device,
        language=language,
        python=python,
        worker=worker,
        revision=revision,
        requirements_lock=requirements_lock,
        model_path=model_path,
        artifacts=artifacts,
        options=dict(options),
        limits=normalized_limits,
        source_path=source_path,
    )
