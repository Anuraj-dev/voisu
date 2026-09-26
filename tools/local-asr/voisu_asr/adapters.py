"""Official local ASR runtime adapters.

The worker imports this module in every runtime environment, but each external
runtime is imported lazily by its adapter. This keeps contract checks runnable
on the base Python installation and prevents an unselected model from changing
startup behavior.
"""

from __future__ import annotations

import contextlib
import ctypes
import gc
import os
import re
import resource
import subprocess
import tempfile
import traceback
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Callable, Iterator

from .audio import pcm_to_float32, write_pcm16_wav
from .candidates import CANDIDATES
from .manifest import (
    COHERE_HF_REV,
    FUN_ASR_DECODER_Q8_NAME,
    FUN_ASR_ENCODER_NAME,
    FUN_ASR_HF_REV,
    FUN_ASR_LLAMA_RUNTIME,
    FUN_ASR_MLT_MARKER,
    FUN_ASR_NANO_CHECKPOINT,
    FUN_ASR_PYTHON_ENGLISH,
    FUN_ASR_VAD_NAME,
    MOONSHINE_MEDIUM_ARCH,
    MOONSHINE_MEDIUM_FILES,
    MOONSHINE_MEDIUM_PINS,
    MOONSHINE_VOICE_SOURCE,
    Manifest,
    ManifestError,
    moonshine_medium_paths,
    verify_moonshine_medium_pins,
)


class AdapterError(RuntimeError):
    """A selected runtime could not load or transcribe."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code
        self.message = message


@dataclass(frozen=True)
class AdapterMetadata:
    model_id: str
    observed_device: str
    resident: bool
    backend: str
    peak_host_ram_bytes: int = 0
    peak_gpu_memory_bytes: int = 0


class BaseAdapter:
    metadata: AdapterMetadata

    def prepare(self) -> AdapterMetadata:
        raise NotImplementedError

    def transcribe(self, pcm: bytes) -> str:
        raise NotImplementedError

    def close(self) -> None:
        return None


_PARAKEET_LANGUAGE_CODES = frozenset({"en", "en-US"})


def _language_code(manifest: Manifest) -> str | None:
    if manifest.language == "auto":
        return None
    return {"en": "en-US", "hi": "hi-IN"}[manifest.language]


_QWEN_LANGUAGES = {"en": "English", "hi": "Hindi"}
_QWEN_DTYPES = frozenset({"auto", "bfloat16", "float16", "float32"})
# RTX 5050 Laptop reports 8151 MiB; 75% is admission, 25% stays desktop reserve.
RTX_5050_TOTAL_VRAM_BYTES = 8151 * 1024 * 1024
VRAM_ADMISSION_NUM = 75
VRAM_ADMISSION_DEN = 100


def resolve_qwen_language(language: str) -> str | None:
    """Map a Qwen manifest language to the Transformers transcription hint.

    English and Hinglish stay selectable. `auto` leaves language detection to
    the model; `hi` forces Hindi rather than translating into English.
    """

    if language == "auto":
        return None
    try:
        return _QWEN_LANGUAGES[language]
    except KeyError as exc:
        raise AdapterError("unsupported_language", "Qwen language must be en, hi, or auto") from exc


def vram_admission_bytes(total_vram_bytes: int) -> int:
    return int(total_vram_bytes) * VRAM_ADMISSION_NUM // VRAM_ADMISSION_DEN


def desktop_reserve_bytes(total_vram_bytes: int) -> int:
    return int(total_vram_bytes) - vram_admission_bytes(total_vram_bytes)


def gpu_profile_fits(
    peak_gpu_bytes: int,
    total_vram_bytes: int,
    free_vram_bytes: int | None = None,
) -> bool:
    """True when peak VRAM fits 75% admission and leaves desktop reserve.

    A CUDA request that would require CPU spill or consume the desktop reserve
    is a blocker, not a fallback.
    """

    if peak_gpu_bytes <= 0:
        return False
    budget = vram_admission_bytes(total_vram_bytes)
    if peak_gpu_bytes > budget:
        return False
    reserve = desktop_reserve_bytes(total_vram_bytes)
    if peak_gpu_bytes > total_vram_bytes - reserve:
        return False
    if free_vram_bytes is not None and peak_gpu_bytes > max(0, free_vram_bytes - reserve):
        return False
    return True


def force_offline_hub() -> None:
    """Inference must load verified local artifacts with no Hugging Face access."""

    os.environ["HF_HUB_OFFLINE"] = "1"
    os.environ["TRANSFORMERS_OFFLINE"] = "1"
    os.environ["HF_DATASETS_OFFLINE"] = "1"
    os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"


def host_rss_bytes() -> int:
    """Current RSS. Not a peak: the CPU-side load spike can already have been freed."""

    try:
        for line in Path("/proc/self/status").read_text(encoding="utf-8").splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    except (OSError, ValueError, IndexError):
        return 0
    return 0


def host_peak_ram_bytes() -> int:
    """Process high-water RSS.

    Linux `ru_maxrss` is kilobytes and survives after tensors move to GPU, unlike
    a later `/proc/self/status` VmRSS sample.
    """

    try:
        return int(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss) * 1024
    except (OSError, ValueError, AttributeError):
        return 0


def qwen_dtype_name(device: str, options: dict[str, Any] | None) -> str:
    settings = options or {}
    requested = settings.get("dtype", "bfloat16" if device.startswith("cuda:") else "float32")
    if requested not in _QWEN_DTYPES:
        raise AdapterError("invalid_setting", "Qwen dtype must be auto, bfloat16, float16, or float32")
    return str(requested)


def resolve_nemo_language_code(model_id: str, language: str, options: dict[str, Any] | None) -> str | None:
    """Map a NeMo manifest language to a recognizer code.

    Parakeet is English-only: a language_code option cannot select Hindi.
    """

    settings = options or {}
    if model_id == "parakeet-tdt-0.6b-v3":
        if language != "en":
            raise AdapterError("unsupported_language", "parakeet-tdt-0.6b-v3 is English only")
        code = settings.get("language_code")
        if code is None:
            return "en-US"
        if not isinstance(code, str) or code not in _PARAKEET_LANGUAGE_CODES:
            raise AdapterError(
                "unsupported_language",
                "parakeet-tdt-0.6b-v3 language_code must be en or en-US",
            )
        return "en-US"
    requested = settings.get("language_code")
    if isinstance(requested, str) and requested:
        return requested
    if language == "auto":
        return None
    return {"en": "en-US", "hi": "hi-IN"}[language]


def _torch_device(model: Any, requested: str) -> str:
    """Return an observed parameter device and reject mixed/CPU fallback."""

    devices: set[str] = set()
    try:
        for parameter in model.parameters():
            devices.add(str(parameter.device))
    except (AttributeError, TypeError):
        devices = set()
    if not devices:
        mapping = getattr(model, "hf_device_map", None)
        if isinstance(mapping, dict):
            devices = {str(value) for value in mapping.values() if value is not None}
    if not devices:
        device = getattr(model, "device", None)
        if device is not None:
            devices.add(str(device))
    if not devices:
        raise AdapterError("backend_device_unknown", "runtime did not expose its loaded device")
    if requested == "cpu":
        if devices != {"cpu"}:
            raise AdapterError("backend_device_mismatch", f"requested CPU but runtime reports {sorted(devices)}")
        return "cpu"
    requested_index = requested.split(":", 1)[1]
    expected = {requested, f"cuda:{requested_index}"}
    if not devices or not devices.issubset(expected):
        raise AdapterError("backend_device_mismatch", f"requested {requested} but runtime reports {sorted(devices)}")
    return requested


def _ensure_cuda(torch: Any, requested: str) -> None:
    if requested.startswith("cuda:"):
        if not bool(torch.cuda.is_available()):
            raise AdapterError("cuda_unavailable", "CUDA was requested but torch reports no CUDA device")
        index = int(requested.split(":", 1)[1])
        count = int(torch.cuda.device_count())
        if index >= count:
            raise AdapterError("cuda_unavailable", f"CUDA device {requested} is unavailable")


def _cuda_memory_bytes(torch: Any, device: str) -> int:
    if not device.startswith("cuda:") or torch is None:
        return 0
    index = int(device.split(":", 1)[1])
    allocated = 0
    try:
        allocated = int(torch.cuda.max_memory_allocated(index))
        allocated = max(allocated, int(torch.cuda.memory_allocated(index)))
    except Exception:
        allocated = 0
    nvml = _nvml_process_gpu_memory_bytes(index)
    if nvml:
        allocated = max(allocated, nvml)
    return allocated


@contextlib.contextmanager
def _cuda_request(torch: Any, device: str) -> Iterator[bool]:
    """Scope one transcription's CUDA memory; the model stays resident.

    Peak stats restart so reported peak GPU memory belongs to this request;
    yields False when that reset failed, so the caller must not report a peak.
    Afterwards the caching allocator's free blocks go back to the driver:
    otherwise reserved memory stays at the high-water mark of the longest clip
    ever transcribed (one 7-minute clip pinned ~7 GiB of an 8 GiB card for
    every later dictation). Callers run the request in a separate function so
    its tensors are already unreferenced here on success; on failure the
    exception chain's frames still hold them, so their locals are cleared first.
    """

    if torch is None or not device.startswith("cuda:"):
        yield True
        return
    peak_reset = False
    with contextlib.suppress(Exception):
        torch.cuda.reset_peak_memory_stats(int(device.split(":", 1)[1]))
        peak_reset = True
    try:
        yield peak_reset
    except BaseException as exc:
        seen: set[int] = set()
        link: BaseException | None = exc
        while link is not None and id(link) not in seen:
            seen.add(id(link))
            traceback.clear_frames(link.__traceback__)
            link = link.__cause__ or link.__context__
        raise
    finally:
        # Cleanup must never replace the request's own result or error.
        with contextlib.suppress(Exception):
            torch.cuda.empty_cache()


def _observe_torch_execution_device(torch: Any, model: Any, requested: str) -> str:
    observed = _torch_device(model, requested)
    if requested.startswith("cuda:") and _cuda_memory_bytes(torch, requested) <= 0:
        raise AdapterError(
            "backend_device_mismatch",
            "requested cuda:0 but this process has no GPU memory (CPU spill)",
        )
    return observed


def _is_cuda_oom(exc: BaseException) -> bool:
    text = str(exc).lower()
    return "out of memory" in text or "cuda oom" in text or "cuda error: out of memory" in text


def _move_inputs(inputs: Any, model: Any, device: str) -> Any:
    dtype = getattr(model, "dtype", None)
    try:
        return inputs.to(device, dtype) if dtype is not None else inputs.to(device)
    except TypeError:
        return inputs.to(device)


def _as_text(value: Any) -> str:
    if isinstance(value, str):
        return value.strip()
    if isinstance(value, dict):
        for key in ("text", "transcription", "transcript", "generated_text"):
            if key in value:
                return _as_text(value[key])
    if isinstance(value, (list, tuple)):
        if not value:
            return ""
        return _as_text(value[0])
    return str(value).strip() if value is not None else ""


_SHA1_HEX = re.compile(r"^[0-9a-f]{40}$")
_FUNASR_CLI_NOISE = re.compile(
    r"^(llama|ggml|encoder|decoder|loading|system|warn|info|debug|main:)",
    re.IGNORECASE,
)


def _joined_option_text(model_path: Path | str, options: dict[str, Any] | None) -> str:
    parts = [str(model_path)]
    for value in (options or {}).values():
        if isinstance(value, str):
            parts.append(value)
    root = Path(model_path)
    if root.is_dir():
        for name in ("configuration.json", "config.yaml", "config.json"):
            candidate = root / name
            if candidate.is_file():
                try:
                    parts.append(candidate.read_text(encoding="utf-8", errors="replace")[:4096])
                except OSError:
                    continue
    return "\n".join(parts)


def fun_asr_identity_error(model_path: Path | str, options: dict[str, Any] | None) -> str | None:
    """Reject Fun-ASR-MLT-Nano and require the requested Nano-2512 checkpoint."""

    settings = options or {}
    blob = _joined_option_text(model_path, settings).lower()
    if FUN_ASR_MLT_MARKER in blob:
        return "Fun-ASR-MLT-Nano is not a substitute for Fun-ASR-Nano-2512"
    checkpoint = settings.get("checkpoint")
    if checkpoint != FUN_ASR_NANO_CHECKPOINT:
        return "fun-asr-nano-2512 requires checkpoint Fun-ASR-Nano-2512"
    return None


def require_fun_asr_nano_identity(manifest: Manifest) -> None:
    if manifest.language != "en":
        raise AdapterError("unsupported_language", "fun-asr-nano-2512 is English-only in this evaluation")
    error = fun_asr_identity_error(manifest.model_path, manifest.options)
    if error:
        raise AdapterError("wrong_checkpoint", error)


def require_english_only(manifest: Manifest, model_id: str) -> None:
    if manifest.language != "en":
        raise AdapterError("unsupported_language", f"{model_id} is English only")


def moonshine_identity_error(device: str, options: dict[str, Any] | None) -> str | None:
    if device != "cpu":
        return "Moonshine Medium is CPU-first; GPU is not a supported runtime profile"
    arch = (options or {}).get("model_arch", MOONSHINE_MEDIUM_ARCH)
    if arch != MOONSHINE_MEDIUM_ARCH:
        return "Moonshine model_arch must be MEDIUM_STREAMING"
    return None


def moonshine_medium_files(model_path: Path) -> tuple[Path, ...]:
    return moonshine_medium_paths(model_path)


def require_moonshine_medium_identity(manifest: Manifest) -> None:
    require_english_only(manifest, "moonshine-streaming-medium")
    error = moonshine_identity_error(manifest.device, manifest.options)
    if error:
        raise AdapterError("invalid_setting", error)
    if not Path(manifest.model_path).is_dir():
        raise AdapterError(
            "wrong_checkpoint",
            "moonshine-streaming-medium model_path must be a directory",
        )
    try:
        verify_moonshine_medium_pins(manifest.model_path)
    except ManifestError as exc:
        message = str(exc)
        code = "artifact_missing" if "requires the official" in message else "wrong_checkpoint"
        raise AdapterError(code, message) from exc


def cohere_revision_error(options: dict[str, Any] | None) -> str | None:
    revision = (options or {}).get("revision")
    if not isinstance(revision, str) or not _SHA1_HEX.fullmatch(revision):
        return "cohere-transcribe-03-2026 requires a pinned 40-hex git revision"
    if revision.startswith("refs/"):
        return "cohere revision must be a pinned git commit, not a floating ref"
    if revision != COHERE_HF_REV:
        return "cohere-transcribe-03-2026 revision must be b1eacc2686a3d08ceaae5f24a88b1d519620bc09"
    return None


def require_cohere_english_identity(manifest: Manifest) -> None:
    require_english_only(manifest, "cohere-transcribe-03-2026")
    error = cohere_revision_error(manifest.options)
    if error:
        raise AdapterError("invalid_setting", error)


def parse_funasr_llama_cli_transcript(stdout: str, stderr: str = "") -> str:
    """Take the last non-log line from llama-funasr-cli as the Transcript."""

    for source in (stdout, stderr):
        lines = [line.strip() for line in source.splitlines() if line.strip()]
        text_lines = [line for line in lines if not _FUNASR_CLI_NOISE.match(line)]
        if text_lines:
            return text_lines[-1]
    return ""


def _cohere_error(exc: BaseException) -> AdapterError:
    text = str(exc).lower()
    if "gated" in text or "401" in text or "403" in text or "access to model" in text:
        return AdapterError(
            "gated_model",
            "cohere-transcribe-03-2026 is Hugging Face gated; the host has no accepted token",
        )
    return AdapterError("model_load_failed", f"Cohere model load failed: {exc}")


def create_fun_asr_adapter(manifest: Manifest) -> BaseAdapter:
    """Select llama.cpp on CPU, Python FunASR or Transformers on CUDA.

    Linux llama.cpp v0.2.6 has no Fun-ASR-Nano CUDA package. A CUDA request
    must use the Python fallback instead of silently running the CPU CLI.
    """

    require_fun_asr_nano_identity(manifest)
    if manifest.device.startswith("cuda:"):
        if manifest.options.get("remote_code"):
            return FunAsrAdapter(manifest)
        return FunAsrHfTransformersAdapter(manifest)
    if manifest.options.get("llama_cli"):
        return FunAsrLlamaCppAdapter(manifest)
    if manifest.options.get("remote_code"):
        return FunAsrAdapter(manifest)
    return FunAsrHfTransformersAdapter(manifest)


_NVML_SUCCESS = 0
_NVML_ERROR_INSUFFICIENT_SIZE = 7
_NVML_VALUE_NOT_AVAILABLE = 0xFFFFFFFFFFFFFFFF


def resident_after_inferences(count: int) -> bool:
    """Load plus one inference is still cold. A second inference without reload proves residency."""

    return count >= 2


def observe_execution_device(
    requested: str,
    *,
    maps_text: str,
    gpu_memory_bytes: int | None,
    cuda_device_index: int | None,
    backend_reports_gpu: bool | None = None,
    after_inference: bool = False,
) -> str:
    """Require process/device evidence. Echoing the requested device is not enough."""

    if requested == "cpu":
        return "cpu"
    if requested != "cuda:0":
        raise AdapterError("backend_device_mismatch", f"unsupported device {requested}")
    mapped = "libggml-cuda" in maps_text or "libcuda.so" in maps_text
    if not mapped:
        raise AdapterError(
            "backend_device_mismatch",
            "requested cuda:0 but CUDA libraries are not mapped",
        )
    if cuda_device_index not in (None, 0):
        raise AdapterError(
            "backend_device_mismatch",
            f"requested cuda:0 but CUDA context is device {cuda_device_index}",
        )
    if gpu_memory_bytes is None or gpu_memory_bytes <= 0:
        raise AdapterError(
            "backend_device_mismatch",
            "requested cuda:0 but this process has no GPU memory (CPU spill)",
        )
    if after_inference and backend_reports_gpu is not True:
        raise AdapterError(
            "backend_device_mismatch",
            "requested cuda:0 but the completed inference did not report a GPU backend",
        )
    return "cuda:0"


def _read_maps() -> str:
    try:
        return Path("/proc/self/maps").read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def _nvml_process_gpu_memory_bytes(device_index: int) -> int | None:
    try:
        nvml = ctypes.CDLL("libnvidia-ml.so.1")
    except OSError:
        return None
    if nvml.nvmlInit_v2() != _NVML_SUCCESS:
        return None

    class Proc(ctypes.Structure):
        _fields_ = [
            ("pid", ctypes.c_uint),
            ("usedGpuMemory", ctypes.c_ulonglong),
            ("gpuInstanceId", ctypes.c_uint),
            ("computeInstanceId", ctypes.c_uint),
        ]

    try:
        handle = ctypes.c_void_p()
        if nvml.nvmlDeviceGetHandleByIndex_v2(device_index, ctypes.byref(handle)) != _NVML_SUCCESS:
            return None
        pid = os.getpid()
        count = ctypes.c_uint(8)
        for _ in range(4):
            infos = (Proc * count.value)()
            status = nvml.nvmlDeviceGetComputeRunningProcesses_v3(
                handle, ctypes.byref(count), infos
            )
            if status == _NVML_SUCCESS:
                used = 0
                for index in range(count.value):
                    if infos[index].pid != pid:
                        continue
                    if infos[index].usedGpuMemory == _NVML_VALUE_NOT_AVAILABLE:
                        continue
                    used += int(infos[index].usedGpuMemory)
                return used
            if status != _NVML_ERROR_INSUFFICIENT_SIZE:
                return None
            count = ctypes.c_uint(max(int(count.value) * 2, 8))
        return None
    finally:
        nvml.nvmlShutdown()


def _cuda_context_device() -> int | None:
    try:
        libcuda = ctypes.CDLL("libcuda.so.1")
    except OSError:
        return None
    if libcuda.cuInit(0) != 0:
        return None
    context = ctypes.c_void_p()
    if libcuda.cuCtxGetCurrent(ctypes.byref(context)) != 0 or not context:
        return None
    device = ctypes.c_int(-1)
    if libcuda.cuCtxGetDevice(ctypes.byref(device)) != 0:
        return None
    return int(device.value)


def _observe_native_device(
    requested: str,
    *,
    after_inference: bool = False,
    backend_reports_gpu: bool | None = None,
) -> str:
    gpu_memory = None
    cuda_device = None
    if requested.startswith("cuda:"):
        gpu_memory = _nvml_process_gpu_memory_bytes(int(requested.split(":", 1)[1]))
        cuda_device = _cuda_context_device()
    return observe_execution_device(
        requested,
        maps_text=_read_maps(),
        gpu_memory_bytes=gpu_memory,
        cuda_device_index=cuda_device,
        backend_reports_gpu=backend_reports_gpu,
        after_inference=after_inference,
    )


def _nvml_vram_snapshot(device_index: int) -> tuple[int, int] | None:
    try:
        nvml = ctypes.CDLL("libnvidia-ml.so.1")
    except OSError:
        return None

    class Mem(ctypes.Structure):
        _fields_ = [
            ("total", ctypes.c_ulonglong),
            ("free", ctypes.c_ulonglong),
            ("used", ctypes.c_ulonglong),
        ]

    if nvml.nvmlInit_v2() != _NVML_SUCCESS:
        return None
    try:
        handle = ctypes.c_void_p()
        if nvml.nvmlDeviceGetHandleByIndex_v2(device_index, ctypes.byref(handle)) != _NVML_SUCCESS:
            return None
        info = Mem()
        if nvml.nvmlDeviceGetMemoryInfo(handle, ctypes.byref(info)) != _NVML_SUCCESS:
            return None
        return int(info.total), int(info.free)
    finally:
        nvml.nvmlShutdown()


def require_qwen_vram_admission(manifest: Manifest, total_vram_bytes: int, free_vram_bytes: int | None) -> None:
    if not manifest.device.startswith("cuda:"):
        return
    peak = int(manifest.limits.get("vram_bytes") or 0)
    if peak <= 0:
        raise AdapterError("vram_admission", "Qwen CUDA requires limits.vram_bytes")
    if not gpu_profile_fits(peak, total_vram_bytes, free_vram_bytes):
        raise AdapterError(
            "vram_admission",
            f"Qwen peak {peak} does not fit VRAM admission total={total_vram_bytes} free={free_vram_bytes}",
        )


class NemoSpeechCAdapter(BaseAdapter):
    """Resident adapter for NVIDIA's official NeMo-Speech.cpp C ABI."""

    class BackendConfig(ctypes.Structure):
        _fields_ = [("size", ctypes.c_size_t), ("gpu", ctypes.c_int32)]

    class ModelConfig(ctypes.Structure):
        _fields_ = [("size", ctypes.c_size_t), ("path", ctypes.c_char_p), ("name", ctypes.c_char_p)]

    class RecognizerConfig(ctypes.Structure):
        _fields_ = [
            ("size", ctypes.c_size_t),
            ("backend", ctypes.c_void_p),
            ("model", ctypes.c_void_p),
            ("streaming", ctypes.c_void_p),
            ("decoder", ctypes.c_void_p),
            ("vad", ctypes.c_void_p),
            ("endpointing", ctypes.c_void_p),
            ("postproc", ctypes.c_void_p),
            ("diar", ctypes.c_void_p),
            ("batching", ctypes.c_void_p),
        ]

    class RecognitionOptions(ctypes.Structure):
        _fields_ = [
            ("size", ctypes.c_size_t),
            ("request_id", ctypes.c_char_p),
            ("language_code", ctypes.c_char_p),
            ("interim_results", ctypes.c_bool),
            ("enable_word_time_offsets", ctypes.c_bool),
            ("enable_automatic_punctuation", ctypes.c_bool),
            ("verbatim_transcripts", ctypes.c_bool),
            ("profanity_filter", ctypes.c_bool),
            ("stop_history_eou_ms", ctypes.c_int32),
            ("speech_contexts", ctypes.c_void_p),
            ("speech_context_count", ctypes.c_size_t),
            ("max_alternatives", ctypes.c_int32),
            ("enable_speaker_diarization", ctypes.c_bool),
            ("max_speaker_count", ctypes.c_int32),
        ]

    def __init__(self, manifest: Manifest):
        self.manifest = manifest
        self._library = None
        self._recognizer = ctypes.c_void_p()
        self._inferences = 0
        self._backend_gpu = manifest.device.startswith("cuda:")
        self.metadata = AdapterMetadata(manifest.model_id, manifest.device, False, "nemo-speech.cpp:c-api")
        self._load()

    def _load(self) -> None:
        library_path = Path(self.manifest.options["library"])
        try:
            library = ctypes.CDLL(str(library_path))
        except OSError as exc:
            raise AdapterError("backend_unavailable", f"cannot load NeMo-Speech.cpp library: {exc}") from exc
        self._library = library
        self._bind_library(library)
        backend = self.BackendConfig(ctypes.sizeof(self.BackendConfig), -1 if self.manifest.device == "cpu" else 0)
        model_path = str(self.manifest.model_path).encode("utf-8")
        model = self.ModelConfig(ctypes.sizeof(self.ModelConfig), model_path, None)
        config = self.RecognizerConfig(
            ctypes.sizeof(self.RecognizerConfig),
            ctypes.cast(ctypes.pointer(backend), ctypes.c_void_p),
            ctypes.cast(ctypes.pointer(model), ctypes.c_void_p),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        status = library.nemo_speech_asr_create(ctypes.byref(config), ctypes.byref(self._recognizer))
        if status != 0 or not self._recognizer:
            raise AdapterError("model_load_failed", self._last_error(library))
        observed = _observe_native_device(self.manifest.device)
        self.metadata = AdapterMetadata(self.manifest.model_id, observed, False, "nemo-speech.cpp:c-api")

    @staticmethod
    def _bind_library(library: Any) -> None:
        library.nemo_speech_asr_create.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p)]
        library.nemo_speech_asr_create.restype = ctypes.c_int
        library.nemo_speech_asr_destroy.argtypes = [ctypes.c_void_p]
        library.nemo_speech_asr_destroy.restype = None
        library.nemo_speech_asr_recognition_options_default.argtypes = []
        library.nemo_speech_asr_recognition_options_default.restype = NemoSpeechCAdapter.RecognitionOptions
        library.nemo_speech_asr_recognize_f32.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(NemoSpeechCAdapter.RecognitionOptions),
            ctypes.POINTER(ctypes.c_float),
            ctypes.c_size_t,
            ctypes.c_int32,
            ctypes.POINTER(ctypes.c_void_p),
        ]
        library.nemo_speech_asr_recognize_f32.restype = ctypes.c_int
        library.nemo_speech_asr_result_alternative_count.argtypes = [ctypes.c_void_p]
        library.nemo_speech_asr_result_alternative_count.restype = ctypes.c_size_t
        library.nemo_speech_asr_result_transcript.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
        library.nemo_speech_asr_result_transcript.restype = ctypes.c_char_p
        library.nemo_speech_asr_result_destroy.argtypes = [ctypes.c_void_p]
        library.nemo_speech_asr_result_destroy.restype = None
        library.nemo_speech_asr_last_error.argtypes = []
        library.nemo_speech_asr_last_error.restype = ctypes.c_char_p

    @staticmethod
    def _last_error(library: Any) -> str:
        try:
            value = library.nemo_speech_asr_last_error()
            return value.decode("utf-8", "replace") if value else "NeMo-Speech.cpp returned an unknown error"
        except Exception:
            return "NeMo-Speech.cpp returned an unknown error"

    def transcribe(self, pcm: bytes) -> str:
        if self._library is None or not self._recognizer:
            raise AdapterError("not_ready", "NeMo-Speech.cpp recognizer is not loaded")
        samples = pcm_to_float32(pcm)
        sample_array = (ctypes.c_float * len(samples))(*samples)
        options = self._library.nemo_speech_asr_recognition_options_default()
        language_code = resolve_nemo_language_code(
            self.manifest.model_id, self.manifest.language, self.manifest.options
        )
        language_buffer = language_code.encode("utf-8") if language_code else None
        options.language_code = language_buffer
        options.interim_results = False
        options.enable_automatic_punctuation = True
        options.verbatim_transcripts = False
        result = ctypes.c_void_p()
        status = self._library.nemo_speech_asr_recognize_f32(
            self._recognizer,
            ctypes.byref(options),
            sample_array,
            len(samples),
            16_000,
            ctypes.byref(result),
        )
        if status != 0:
            raise AdapterError("inference_failed", self._last_error(self._library))
        try:
            count = self._library.nemo_speech_asr_result_alternative_count(result)
            if count == 0:
                text = ""
            else:
                raw = self._library.nemo_speech_asr_result_transcript(result, 0)
                text = raw.decode("utf-8", "replace") if raw else ""
            observed = _observe_native_device(
                self.manifest.device,
                after_inference=True,
                backend_reports_gpu=self._backend_gpu,
            )
            self._inferences += 1
            self.metadata = AdapterMetadata(
                self.manifest.model_id,
                observed,
                resident_after_inferences(self._inferences),
                "nemo-speech.cpp:c-api",
            )
            return text
        finally:
            self._library.nemo_speech_asr_result_destroy(result)

    def close(self) -> None:
        if self._library is not None and self._recognizer:
            self._library.nemo_speech_asr_destroy(self._recognizer)
            self._recognizer = ctypes.c_void_p()
        self._library = None
        self._inferences = 0


class QwenTransformersAdapter(BaseAdapter):
    """Qwen3-ASR official Transformers adapter for both model sizes.

    Uses the pinned Transformers backend only. A CUDA request must not spill
    to CPU when the profile does not fit.
    """

    def __init__(self, manifest: Manifest):
        self.manifest = manifest
        self.model = None
        self.processor = None
        self.torch = None
        self._inferences = 0
        self._peak_host_ram_bytes = host_peak_ram_bytes()
        self._peak_gpu_memory_bytes = 0
        self.metadata = AdapterMetadata(
            manifest.model_id,
            manifest.device,
            False,
            "transformers:Qwen3-ASR",
            self._peak_host_ram_bytes,
            0,
        )
        self._load()

    def _record_peaks(self) -> None:
        self._peak_host_ram_bytes = max(self._peak_host_ram_bytes, host_peak_ram_bytes())
        if self.torch is not None:
            self._peak_gpu_memory_bytes = max(
                self._peak_gpu_memory_bytes,
                _cuda_memory_bytes(self.torch, self.manifest.device),
            )

    def _metadata(self, observed: str) -> AdapterMetadata:
        self._record_peaks()
        return AdapterMetadata(
            self.manifest.model_id,
            observed,
            resident_after_inferences(self._inferences),
            "transformers:Qwen3-ASR",
            self._peak_host_ram_bytes,
            self._peak_gpu_memory_bytes,
        )

    def _load(self) -> None:
        force_offline_hub()
        try:
            import torch
            from transformers import AutoModelForMultimodalLM, AutoProcessor
        except ImportError as exc:
            raise AdapterError("runtime_missing", "Qwen requires the pinned torch and transformers environment") from exc
        _ensure_cuda(torch, self.manifest.device)
        if self.manifest.device.startswith("cuda:"):
            snapshot = _nvml_vram_snapshot(int(self.manifest.device.split(":", 1)[1]))
            if snapshot is None:
                raise AdapterError("cuda_unavailable", "cannot read VRAM for Qwen admission")
            require_qwen_vram_admission(self.manifest, snapshot[0], snapshot[1])
        dtype_name = qwen_dtype_name(self.manifest.device, self.manifest.options)
        dtype = {"auto": "auto", "bfloat16": torch.bfloat16, "float16": torch.float16, "float32": torch.float32}[dtype_name]
        # Load onto the requested device with `.to(...)`. An automatic
        # device_map can silently CPU-spill and is forbidden for a CUDA request.
        model_kwargs: dict[str, Any] = {"local_files_only": True}
        if dtype != "auto":
            model_kwargs["dtype"] = dtype
        self.torch = torch
        if self.manifest.device.startswith("cuda:"):
            torch.cuda.reset_peak_memory_stats()
        try:
            self.processor = AutoProcessor.from_pretrained(str(self.manifest.model_path), local_files_only=True)
            self.model = AutoModelForMultimodalLM.from_pretrained(str(self.manifest.model_path), **model_kwargs)
            self._record_peaks()
            self.model.to(self.manifest.device)
            self._record_peaks()
            self.model.eval()
            observed = _observe_torch_execution_device(torch, self.model, self.manifest.device)
        except AdapterError:
            raise
        except Exception as exc:
            if _is_cuda_oom(exc):
                self._record_peaks()
                raise AdapterError(
                    "oom",
                    f"Qwen CUDA OOM on {self.manifest.device} peak_gpu={self._peak_gpu_memory_bytes}: {exc}",
                ) from exc
            raise AdapterError("model_load_failed", f"Qwen model load failed: {exc}") from exc
        self.metadata = self._metadata(observed)

    def transcribe(self, pcm: bytes) -> str:
        self._peak_gpu_memory_bytes = 0
        with _cuda_request(self.torch, self.manifest.device) as peak_reset:
            text = self._transcribe(pcm)
        if not peak_reset:
            # A stale lifetime peak must not pass as this request's; 0 = unknown.
            self.metadata = replace(self.metadata, peak_gpu_memory_bytes=0)
        return text

    def _transcribe(self, pcm: bytes) -> str:
        if self.model is None or self.processor is None or self.torch is None:
            raise AdapterError("not_ready", "Qwen model is not loaded")
        force_offline_hub()
        language = resolve_qwen_language(self.manifest.language)
        try:
            import numpy as np

            audio = np.asarray(pcm_to_float32(pcm), dtype=np.float32)
            inputs = self.processor.apply_transcription_request(audio=audio, language=language)
            inputs = _move_inputs(inputs, self.model, self.metadata.observed_device)
            with self.torch.inference_mode():
                output_ids = self.model.generate(
                    **inputs,
                    max_new_tokens=int(self.manifest.options.get("max_new_tokens", 256)),
                    do_sample=False,
                )
            self._record_peaks()
            generated_ids = output_ids[:, inputs["input_ids"].shape[1] :]
            text = _as_text(self.processor.decode(generated_ids, return_format="transcription_only"))
            observed = _observe_torch_execution_device(self.torch, self.model, self.manifest.device)
            self._inferences += 1
            self.metadata = self._metadata(observed)
            return text
        except AdapterError:
            raise
        except Exception as exc:
            if _is_cuda_oom(exc):
                self._record_peaks()
                raise AdapterError(
                    "oom",
                    f"Qwen CUDA OOM during inference peak_gpu={self._peak_gpu_memory_bytes}: {exc}",
                ) from exc
            raise AdapterError("inference_failed", f"Qwen inference failed: {exc}") from exc

    def close(self) -> None:
        self.model = None
        self.processor = None
        self._inferences = 0
        if self.torch is not None and self.manifest.device.startswith("cuda:"):
            self.torch.cuda.empty_cache()
        gc.collect()


class FunAsrLlamaCppAdapter(BaseAdapter):
    """Fun-ASR-Nano-2512 through the documented llama.cpp encoder/decoder bundle.

    Linux prebuilt packages are CPU (and Vulkan for SenseVoiceSmall, not Nano).
    A CUDA request must use the Python FunASR fallback instead of silent CPU spill.
    A CLI invocation reloads weights, so residency stays false.
    """

    def __init__(self, manifest: Manifest):
        require_fun_asr_nano_identity(manifest)
        if manifest.device != "cpu":
            raise AdapterError(
                "native_cuda_unsuitable",
                "Linux FunASR llama.cpp v0.2.6 has no Fun-ASR-Nano CUDA package; use the Python FunASR fallback",
            )
        self.manifest = manifest
        self._inferences = 0
        self.cli = Path(str(manifest.options["llama_cli"]))
        self.encoder = Path(str(manifest.options["encoder"]))
        self.decoder = Path(str(manifest.options["decoder"]))
        vad = manifest.options.get("vad")
        self.vad = Path(str(vad)) if isinstance(vad, str) else None
        for label, path in (("llama_cli", self.cli), ("encoder", self.encoder), ("decoder", self.decoder)):
            if not path.is_file():
                raise AdapterError("artifact_missing", f"Fun-ASR native {label} is not a regular file")
        if FUN_ASR_MLT_MARKER in str(self.encoder).lower() or FUN_ASR_MLT_MARKER in str(self.decoder).lower():
            raise AdapterError("wrong_checkpoint", "Fun-ASR-MLT-Nano is not a substitute for Fun-ASR-Nano-2512")
        if self.encoder.name != FUN_ASR_ENCODER_NAME:
            raise AdapterError("wrong_checkpoint", f"Fun-ASR encoder must be {FUN_ASR_ENCODER_NAME}")
        if self.decoder.name != FUN_ASR_DECODER_Q8_NAME:
            raise AdapterError("wrong_checkpoint", f"Fun-ASR decoder must be {FUN_ASR_DECODER_Q8_NAME}")
        self.metadata = AdapterMetadata(manifest.model_id, "cpu", False, "funasr:llama.cpp")

    def transcribe(self, pcm: bytes) -> str:
        with tempfile.TemporaryDirectory(prefix="voisu-funasr-") as temp:
            wav_path = Path(temp) / "audio.wav"
            write_pcm16_wav(wav_path, pcm)
            command = [
                str(self.cli),
                "--enc",
                str(self.encoder),
                "-m",
                str(self.decoder),
                "-a",
                str(wav_path),
            ]
            if self.vad is not None:
                command.extend(["--vad", str(self.vad)])
            try:
                completed = subprocess.run(
                    command,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=45,
                )
            except (OSError, subprocess.TimeoutExpired) as exc:
                raise AdapterError("inference_failed", f"Fun-ASR llama.cpp failed: {exc}") from exc
            if completed.returncode != 0:
                raise AdapterError(
                    "inference_failed",
                    f"Fun-ASR llama.cpp exited {completed.returncode}: {completed.stderr[-400:]}",
                )
            text = parse_funasr_llama_cli_transcript(completed.stdout, completed.stderr)
            self._inferences += 1
            self.metadata = AdapterMetadata(
                self.manifest.model_id,
                "cpu",
                False,
                "funasr:llama.cpp",
            )
            return text

    def close(self) -> None:
        self._inferences = 0


class FunAsrAdapter(BaseAdapter):
    """Fun-ASR-Nano-2512 Python FunASR fallback when native CUDA is unsuitable."""

    def __init__(self, manifest: Manifest):
        require_fun_asr_nano_identity(manifest)
        self.manifest = manifest
        self.model = None
        self.torch = None
        self._inferences = 0
        self.metadata = AdapterMetadata(manifest.model_id, manifest.device, False, "funasr:AutoModel")
        self._load()

    def _load(self) -> None:
        force_offline_hub()
        try:
            import torch
            from funasr import AutoModel
        except ImportError as exc:
            raise AdapterError("runtime_missing", "Fun-ASR requires the pinned FunASR environment") from exc
        remote_code = self.manifest.options.get("remote_code")
        if not isinstance(remote_code, str) or not Path(remote_code).is_absolute() or not Path(remote_code).is_file():
            raise AdapterError("artifact_missing", "Fun-ASR remote_code must be a pinned local model.py")
        _ensure_cuda(torch, self.manifest.device)
        try:
            self.model = AutoModel(
                model=str(self.manifest.model_path),
                trust_remote_code=True,
                remote_code=remote_code,
                device=self.manifest.device,
                hub=self.manifest.options.get("hub", "hf"),
            )
            inner = getattr(self.model, "model", self.model)
            observed = _observe_torch_execution_device(torch, inner, self.manifest.device)
        except AdapterError:
            raise
        except Exception as exc:
            raise AdapterError("model_load_failed", f"Fun-ASR model load failed: {exc}") from exc
        self.torch = torch
        self.metadata = AdapterMetadata(self.manifest.model_id, observed, False, "funasr:AutoModel")

    def transcribe(self, pcm: bytes) -> str:
        with _cuda_request(self.torch, self.manifest.device):
            return self._transcribe(pcm)

    def _transcribe(self, pcm: bytes) -> str:
        if self.model is None:
            raise AdapterError("not_ready", "Fun-ASR model is not loaded")
        force_offline_hub()
        with tempfile.TemporaryDirectory(prefix="voisu-funasr-") as temp:
            wav_path = Path(temp) / "audio.wav"
            write_pcm16_wav(wav_path, pcm)
            try:
                result = self.model.generate(
                    input=[str(wav_path)],
                    cache={},
                    batch_size=1,
                    language=FUN_ASR_PYTHON_ENGLISH,
                    itn=bool(self.manifest.options.get("itn", True)),
                )
                inner = getattr(self.model, "model", self.model)
                observed = _observe_torch_execution_device(self.torch, inner, self.manifest.device)
                self._inferences += 1
                self.metadata = AdapterMetadata(
                    self.manifest.model_id,
                    observed,
                    resident_after_inferences(self._inferences),
                    "funasr:AutoModel",
                )
                return _as_text(result)
            except AdapterError:
                raise
            except Exception as exc:
                raise AdapterError("inference_failed", f"Fun-ASR inference failed: {exc}") from exc

    def close(self) -> None:
        self.model = None
        self._inferences = 0
        if self.torch is not None and self.manifest.device.startswith("cuda:"):
            self.torch.cuda.empty_cache()
        gc.collect()


class FunAsrHfTransformersAdapter(BaseAdapter):
    """Fun-ASR-Nano-2512-hf through official Transformers 5.17.0.

    This is the CUDA Python fallback when Linux llama.cpp has no Nano GPU
    package. Fun-ASR-MLT-Nano is rejected. Hindi is not selectable.
    """

    def __init__(self, manifest: Manifest):
        require_fun_asr_nano_identity(manifest)
        revision = manifest.options.get("revision", FUN_ASR_HF_REV)
        if revision != FUN_ASR_HF_REV:
            raise AdapterError(
                "wrong_checkpoint",
                "fun-asr-nano-2512 Transformers path requires Fun-ASR-Nano-2512-hf revision "
                f"{FUN_ASR_HF_REV}",
            )
        self.manifest = manifest
        self.model = None
        self.processor = None
        self.torch = None
        self._inferences = 0
        self._peak_host_ram_bytes = host_peak_ram_bytes()
        self._peak_gpu_memory_bytes = 0
        self.metadata = AdapterMetadata(
            manifest.model_id,
            manifest.device,
            False,
            "transformers:Fun-ASR-Nano",
            self._peak_host_ram_bytes,
            0,
        )
        self._load()

    def _record_peaks(self) -> None:
        self._peak_host_ram_bytes = max(self._peak_host_ram_bytes, host_peak_ram_bytes())
        if self.torch is not None:
            self._peak_gpu_memory_bytes = max(
                self._peak_gpu_memory_bytes,
                _cuda_memory_bytes(self.torch, self.manifest.device),
            )

    def _metadata(self, observed: str) -> AdapterMetadata:
        self._record_peaks()
        return AdapterMetadata(
            self.manifest.model_id,
            observed,
            resident_after_inferences(self._inferences),
            "transformers:Fun-ASR-Nano",
            self._peak_host_ram_bytes,
            self._peak_gpu_memory_bytes,
        )

    def _load(self) -> None:
        force_offline_hub()
        try:
            import torch
            from transformers import AutoModelForSpeechSeq2Seq, AutoProcessor
        except ImportError as exc:
            raise AdapterError(
                "runtime_missing",
                "Fun-ASR-Nano-2512-hf requires the pinned torch, torchaudio, and transformers environment",
            ) from exc
        _ensure_cuda(torch, self.manifest.device)
        dtype = torch.bfloat16 if self.manifest.device.startswith("cuda:") else torch.float32
        requested = self.manifest.options.get("dtype")
        if requested in {"float32", "float16", "bfloat16"}:
            dtype = {"float32": torch.float32, "float16": torch.float16, "bfloat16": torch.bfloat16}[requested]
        self.torch = torch
        if self.manifest.device.startswith("cuda:"):
            torch.cuda.reset_peak_memory_stats()
        revision = str(self.manifest.options.get("revision", FUN_ASR_HF_REV))
        try:
            self.processor = AutoProcessor.from_pretrained(
                str(self.manifest.model_path),
                revision=revision,
                local_files_only=True,
                trust_remote_code=False,
            )
            self.model = AutoModelForSpeechSeq2Seq.from_pretrained(
                str(self.manifest.model_path),
                revision=revision,
                local_files_only=True,
                trust_remote_code=False,
                dtype=dtype,
            )
            self._record_peaks()
            self.model.to(self.manifest.device)
            self.model.eval()
            observed = _observe_torch_execution_device(torch, self.model, self.manifest.device)
        except AdapterError:
            raise
        except Exception as exc:
            if _is_cuda_oom(exc):
                self._record_peaks()
                raise AdapterError(
                    "oom",
                    f"Fun-ASR CUDA OOM on {self.manifest.device} peak_gpu={self._peak_gpu_memory_bytes}: {exc}",
                ) from exc
            raise AdapterError("model_load_failed", f"Fun-ASR-Nano-2512-hf load failed: {exc}") from exc
        self.metadata = self._metadata(observed)

    def transcribe(self, pcm: bytes) -> str:
        self._peak_gpu_memory_bytes = 0
        with _cuda_request(self.torch, self.manifest.device) as peak_reset:
            text = self._transcribe(pcm)
        if not peak_reset:
            # A stale lifetime peak must not pass as this request's; 0 = unknown.
            self.metadata = replace(self.metadata, peak_gpu_memory_bytes=0)
        return text

    def _transcribe(self, pcm: bytes) -> str:
        if self.model is None or self.processor is None or self.torch is None:
            raise AdapterError("not_ready", "Fun-ASR-Nano-2512-hf is not loaded")
        force_offline_hub()
        try:
            import numpy as np

            audio = np.asarray(pcm_to_float32(pcm), dtype=np.float32)
            inputs = self.processor.apply_transcription_request(
                audio=audio,
                language="en",
                processor_kwargs={
                    "return_tensors": "pt",
                    "audio_kwargs": {"sampling_rate": 16_000},
                    "text_kwargs": {"padding": True},
                },
            )
            inputs = _move_inputs(inputs, self.model, self.metadata.observed_device)
            with self.torch.inference_mode():
                generated = self.model.generate(
                    **inputs,
                    max_new_tokens=int(self.manifest.options.get("max_new_tokens", 128)),
                    do_sample=False,
                )
            self._record_peaks()
            new_tokens = generated[:, inputs.input_ids.shape[1] :]
            text = _as_text(self.processor.batch_decode(new_tokens, skip_special_tokens=True))
            observed = _observe_torch_execution_device(self.torch, self.model, self.manifest.device)
            self._inferences += 1
            self.metadata = self._metadata(observed)
            return text
        except AdapterError:
            raise
        except Exception as exc:
            if _is_cuda_oom(exc):
                self._record_peaks()
                raise AdapterError(
                    "oom",
                    f"Fun-ASR CUDA OOM during inference peak_gpu={self._peak_gpu_memory_bytes}: {exc}",
                ) from exc
            raise AdapterError("inference_failed", f"Fun-ASR-Nano-2512-hf inference failed: {exc}") from exc

    def close(self) -> None:
        self.model = None
        self.processor = None
        self._inferences = 0
        if self.torch is not None and self.manifest.device.startswith("cuda:"):
            self.torch.cuda.empty_cache()
        gc.collect()


class MoonshineAdapter(BaseAdapter):
    """Moonshine Voice Medium Streaming, CPU-first, final-only calls."""

    def __init__(self, manifest: Manifest):
        require_moonshine_medium_identity(manifest)
        self.manifest = manifest
        self.transcriber = None
        self._inferences = 0
        self.metadata = AdapterMetadata(manifest.model_id, "cpu", False, "moonshine-voice:Transcriber")
        self._load()

    def _load(self) -> None:
        try:
            from moonshine_voice.moonshine_api import ModelArch
            from moonshine_voice.transcriber import Transcriber
        except ImportError as exc:
            raise AdapterError("runtime_missing", "Moonshine requires the pinned moonshine-voice environment") from exc
        arch = getattr(ModelArch, MOONSHINE_MEDIUM_ARCH)
        try:
            self.transcriber = Transcriber(model_path=self.manifest.model_path, model_arch=arch)
        except Exception as exc:
            raise AdapterError("model_load_failed", f"Moonshine model load failed: {exc}") from exc

    def transcribe(self, pcm: bytes) -> str:
        if self.transcriber is None:
            raise AdapterError("not_ready", "Moonshine transcriber is not loaded")
        try:
            result = self.transcriber.transcribe_without_streaming(pcm_to_float32(pcm), sample_rate=16_000)
            self._inferences += 1
            self.metadata = AdapterMetadata(
                self.manifest.model_id,
                "cpu",
                resident_after_inferences(self._inferences),
                "moonshine-voice:Transcriber",
            )
            return " ".join(str(line.text).strip() for line in result.lines if str(line.text).strip())
        except Exception as exc:
            raise AdapterError("inference_failed", f"Moonshine inference failed: {exc}") from exc

    def close(self) -> None:
        if self.transcriber is not None:
            self.transcriber.close()
            self.transcriber = None
        self._inferences = 0


class CohereTransformersAdapter(BaseAdapter):
    """Cohere Transcribe 03-2026 English-only Transformers adapter."""

    def __init__(self, manifest: Manifest):
        require_cohere_english_identity(manifest)
        self.manifest = manifest
        self.model = None
        self.processor = None
        self.torch = None
        self.numpy = None
        self._inferences = 0
        self.metadata = AdapterMetadata(manifest.model_id, manifest.device, False, "transformers:CohereAsr")
        self._load()

    def _load(self) -> None:
        force_offline_hub()
        try:
            import numpy as np
            import torch
            from transformers import AutoProcessor, CohereAsrForConditionalGeneration
        except ImportError as exc:
            raise AdapterError("runtime_missing", "Cohere requires the pinned torch, numpy, and transformers environment") from exc
        _ensure_cuda(torch, self.manifest.device)
        revision = str(self.manifest.options["revision"])
        model_kwargs: dict[str, Any] = {
            "local_files_only": True,
            "revision": revision,
        }
        try:
            self.processor = AutoProcessor.from_pretrained(
                str(self.manifest.model_path),
                revision=revision,
                local_files_only=True,
            )
            self.model = CohereAsrForConditionalGeneration.from_pretrained(
                str(self.manifest.model_path),
                **model_kwargs,
            )
            self.model.to(self.manifest.device)
            self.model.eval()
            observed = _observe_torch_execution_device(torch, self.model, self.manifest.device)
        except AdapterError:
            raise
        except Exception as exc:
            raise _cohere_error(exc) from exc
        self.torch = torch
        self.numpy = np
        self.metadata = AdapterMetadata(self.manifest.model_id, observed, False, "transformers:CohereAsr")

    def transcribe(self, pcm: bytes) -> str:
        with _cuda_request(self.torch, self.manifest.device):
            return self._transcribe(pcm)

    def _transcribe(self, pcm: bytes) -> str:
        if self.model is None or self.processor is None or self.torch is None or self.numpy is None:
            raise AdapterError("not_ready", "Cohere model is not loaded")
        force_offline_hub()
        audio = self.numpy.frombuffer(pcm, dtype=self.numpy.int16).astype(self.numpy.float32) / 32768.0
        language = "en"
        try:
            inputs = self.processor(
                audio,
                sampling_rate=16_000,
                return_tensors="pt",
                language=language,
                punctuation=bool(self.manifest.options.get("punctuation", True)),
            )
            inputs = _move_inputs(inputs, self.model, self.metadata.observed_device)
            with self.torch.inference_mode():
                outputs = self.model.generate(
                    **inputs,
                    max_new_tokens=int(self.manifest.options.get("max_new_tokens", 256)),
                )
            decode_kwargs = {"skip_special_tokens": True}
            chunk_index = inputs.get("audio_chunk_index") if hasattr(inputs, "get") else None
            if chunk_index is not None:
                decode_kwargs.update({"audio_chunk_index": chunk_index, "language": language})
            text = _as_text(self.processor.decode(outputs, **decode_kwargs))
            observed = _observe_torch_execution_device(self.torch, self.model, self.manifest.device)
            self._inferences += 1
            self.metadata = AdapterMetadata(
                self.manifest.model_id,
                observed,
                resident_after_inferences(self._inferences),
                "transformers:CohereAsr",
            )
            return text
        except AdapterError:
            raise
        except Exception as exc:
            raise AdapterError("inference_failed", f"Cohere inference failed: {exc}") from exc

    def close(self) -> None:
        self.model = None
        self.processor = None
        self._inferences = 0
        if self.torch is not None and self.manifest.device.startswith("cuda:"):
            self.torch.cuda.empty_cache()
        gc.collect()


AdapterLoader = Callable[[Manifest], BaseAdapter]


ADAPTER_LOADERS: dict[str, AdapterLoader] = {
    "nemotron-3.5-0.6b": NemoSpeechCAdapter,
    "parakeet-tdt-0.6b-v3": NemoSpeechCAdapter,
    "qwen3-asr-0.6b": QwenTransformersAdapter,
    "qwen3-asr-1.7b": QwenTransformersAdapter,
    "fun-asr-nano-2512": create_fun_asr_adapter,
    "moonshine-streaming-medium": MoonshineAdapter,
    "cohere-transcribe-03-2026": CohereTransformersAdapter,
}


def _candidate_adapter(manifest: Manifest) -> BaseAdapter:
    # Low-VRAM candidates (whisper.cpp, CTranslate2, Granite CTC) live in their
    # own module, which imports this one; load it on first use.
    from .candidate_adapters import create_candidate_adapter

    return create_candidate_adapter(manifest)


ADAPTER_LOADERS.update({model_id: _candidate_adapter for model_id in CANDIDATES})


def create_adapter(manifest: Manifest, loaders: dict[str, AdapterLoader] | None = None) -> BaseAdapter:
    loader_map = ADAPTER_LOADERS if loaders is None else loaders
    try:
        loader = loader_map[manifest.model_id]
    except KeyError as exc:
        raise AdapterError("unsupported_model", f"no adapter exists for {manifest.model_id}") from exc
    return loader(manifest)
