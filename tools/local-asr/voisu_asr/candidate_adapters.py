"""Resident adapters for the low-VRAM Local ASR candidates.

Three backends, each keeping one model loaded across requests:

- whisper.cpp through the `pywhispercpp` binding (native GGML, no torch);
- CTranslate2 through `faster-whisper` (native, no torch);
- Granite Speech TurboCTC through Transformers.

The two Whisper backends share language mapping, CUDA library preloading,
VRAM admission, and device observation. A CUDA request never falls back to
CPU: a runtime that did not put the model on the GPU fails as a device
mismatch. Decoding is repeatable (temperature 0.0 without fallback; beam 5
for CTranslate2, greedy for whisper.cpp) and runs only at the measured
precision. Native stdout noise is kept off the
protocol stream by `worker.py`, which points fd 1 at stderr for the whole
worker lifetime.
"""

from __future__ import annotations

import ctypes
import gc
import sysconfig
from pathlib import Path
from typing import Any

from .adapters import (
    AdapterError,
    AdapterMetadata,
    BaseAdapter,
    _cuda_memory_bytes,
    _cuda_request,
    _ensure_cuda,
    _move_inputs,
    _nvml_process_gpu_memory_bytes,
    _nvml_vram_snapshot,
    _observe_native_device,
    _observe_torch_execution_device,
    force_offline_hub,
    gpu_profile_fits,
    host_peak_ram_bytes,
    resident_after_inferences,
)
from .audio import pcm_to_float32
from .candidates import (
    BEAM_SIZE,
    BEST_OF,
    CANDIDATES,
    CTRANSLATE2,
    TRANSFORMERS_CTC,
    WHISPER_CPP,
    WHISPER_CPP_BEST_OF,
    Candidate,
    candidate_error,
)
from .manifest import Manifest

# CUDA user-space libraries shipped as pip wheels inside each runtime venv.
# They are preloaded by absolute path so the extension resolves them by
# soname without LD_LIBRARY_PATH (the worker unit does not set one).
_WHISPER_CPP_CUDA_LIBS = ("cu13/lib/libcudart.so.13", "cu13/lib/libcublasLt.so.13", "cu13/lib/libcublas.so.13")
_CT2_CUDA_LIBS = (
    "cuda_runtime/lib/libcudart.so.12",
    "cublas/lib/libcublasLt.so.12",
    "cublas/lib/libcublas.so.12",
    "cudnn/lib/libcudnn.so.9",
)


def preload_cuda_libraries(relative: tuple[str, ...], root: Path | None = None) -> None:
    base = root if root is not None else Path(sysconfig.get_paths()["purelib"]) / "nvidia"
    for name in relative:
        path = base / name
        try:
            ctypes.CDLL(str(path), mode=ctypes.RTLD_GLOBAL)
        except OSError as exc:
            raise AdapterError("runtime_missing", f"CUDA library is missing from the runtime: {path}") from exc


def candidate_for(manifest: Manifest, backend: str) -> Candidate:
    """Re-check the measured identity even when a Manifest skipped load_manifest."""

    item = CANDIDATES.get(manifest.model_id)
    if item is None or item.backend != backend:
        raise AdapterError("unsupported_model", f"{manifest.model_id} is not a {backend} candidate")
    error = candidate_error(
        item,
        manifest.device,
        manifest.language,
        manifest.model_path,
        manifest.options,
        int(manifest.limits.get("vram_bytes") or 0),
        ((artifact.path, artifact.sha256, artifact.bytes) for artifact in manifest.artifacts),
    )
    if error:
        raise AdapterError("wrong_checkpoint", error)
    return item


def require_vram_admission(manifest: Manifest) -> None:
    """A CUDA manifest must fit the 75% admission budget before anything loads."""

    if not manifest.device.startswith("cuda:"):
        return
    snapshot = _nvml_vram_snapshot(int(manifest.device.split(":", 1)[1]))
    if snapshot is None:
        raise AdapterError("cuda_unavailable", "cannot read VRAM for admission")
    peak = int(manifest.limits.get("vram_bytes") or 0)
    if not gpu_profile_fits(peak, snapshot[0], snapshot[1]):
        raise AdapterError(
            "vram_admission",
            f"{manifest.model_id} peak {peak} does not fit VRAM admission total={snapshot[0]} free={snapshot[1]}",
        )


def _threads(manifest: Manifest) -> int:
    requested = manifest.options.get("n_threads")
    cores = int(manifest.limits.get("cpu_cores") or 1)
    if isinstance(requested, int) and not isinstance(requested, bool) and requested > 0:
        return min(requested, cores)
    return cores


class _NativeWhisperAdapter(BaseAdapter):
    """Shared lifecycle for native Whisper runtimes: load once, observe, count requests."""

    backend = ""

    def __init__(self, manifest: Manifest, candidate: Candidate):
        self.manifest = manifest
        self.candidate = candidate
        self.language = candidate.whisper_language(manifest.language)
        self._inferences = 0
        self._peak_gpu = 0
        self.metadata = AdapterMetadata(manifest.model_id, manifest.device, False, self.backend)
        require_vram_admission(manifest)
        try:
            self._load()
        except AdapterError:
            raise
        except Exception as exc:
            raise AdapterError("model_load_failed", f"{self.backend} model load failed: {exc}") from exc
        self.metadata = self._observe(after_inference=False)

    def _load(self) -> None:
        raise NotImplementedError

    def _run(self, audio: Any) -> str:
        raise NotImplementedError

    def _backend_reports_gpu(self) -> bool:
        raise NotImplementedError

    def _observe(self, *, after_inference: bool) -> AdapterMetadata:
        observed = _observe_native_device(
            self.manifest.device,
            after_inference=after_inference,
            backend_reports_gpu=self._backend_reports_gpu() if after_inference else None,
        )
        gpu = 0
        if observed.startswith("cuda:"):
            gpu = int(_nvml_process_gpu_memory_bytes(int(observed.split(":", 1)[1])) or 0)
        self._peak_gpu = max(self._peak_gpu, gpu)
        return AdapterMetadata(
            self.manifest.model_id,
            observed,
            resident_after_inferences(self._inferences),
            self.backend,
            host_peak_ram_bytes(),
            self._peak_gpu,
        )

    def transcribe(self, pcm: bytes) -> str:
        import numpy as np

        audio = np.asarray(pcm_to_float32(pcm), dtype=np.float32)
        try:
            text = self._run(audio)
        except AdapterError:
            raise
        except Exception as exc:
            code = "oom" if "out of memory" in str(exc).lower() else "inference_failed"
            raise AdapterError(code, f"{self.backend} inference failed: {exc}") from exc
        self._inferences += 1
        self.metadata = self._observe(after_inference=True)
        return " ".join(text.split())


class WhisperCppAdapter(_NativeWhisperAdapter):
    """whisper.cpp GGML model held in one `pywhispercpp` context."""

    backend = "whisper.cpp:pywhispercpp"

    def __init__(self, manifest: Manifest):
        candidate = candidate_for(manifest, WHISPER_CPP)
        if not manifest.model_path.is_file() or manifest.model_path.name != candidate.weights:
            raise AdapterError("artifact_missing", f"whisper.cpp model_path must be {candidate.weights}")
        self.model = None
        self._system_info = ""
        super().__init__(manifest, candidate)

    def _load(self) -> None:
        cuda = self.manifest.device.startswith("cuda:")
        if cuda:
            preload_cuda_libraries(_WHISPER_CPP_CUDA_LIBS)
        try:
            from pywhispercpp.model import Model
        except ImportError as exc:
            raise AdapterError("runtime_missing", "whisper.cpp requires the pinned pywhispercpp runtime") from exc
        self._system_info = str(Model.system_info() or "")
        if cuda and "CUDA" not in self._system_info:
            raise AdapterError("backend_device_mismatch", "pywhispercpp was not built with CUDA")
        self.model = Model(
            str(self.manifest.model_path),
            params_sampling_strategy=0,
            redirect_whispercpp_logs_to=False,
            context_params={"use_gpu": cuda, "flash_attn": False, "gpu_device": 0},
            n_threads=_threads(self.manifest),
            language=self.language or "auto",
            detect_language=False,
            greedy={"best_of": WHISPER_CPP_BEST_OF},
            temperature=0.0,
            temperature_inc=0.0,
            print_progress=False,
            print_realtime=False,
            print_timestamps=False,
            no_context=True,
            suppress_nst=True,
        )

    def _backend_reports_gpu(self) -> bool:
        return "CUDA" in self._system_info

    def _run(self, audio: Any) -> str:
        if self.model is None:
            raise AdapterError("not_ready", "whisper.cpp model is not loaded")
        segments = self.model.transcribe(audio)
        return " ".join(segment.text.strip() for segment in segments if segment.text.strip())

    def close(self) -> None:
        self.model = None
        self._inferences = 0
        gc.collect()


class FasterWhisperAdapter(_NativeWhisperAdapter):
    """CTranslate2 Whisper model through faster-whisper; weights quantize on load."""

    backend = "ctranslate2:faster-whisper"

    def __init__(self, manifest: Manifest):
        candidate = candidate_for(manifest, CTRANSLATE2)
        if not (manifest.model_path / candidate.weights).is_file():
            raise AdapterError("artifact_missing", f"CTranslate2 model directory must contain {candidate.weights}")
        self.model = None
        super().__init__(manifest, candidate)

    def _load(self) -> None:
        cuda = self.manifest.device.startswith("cuda:")
        if cuda:
            preload_cuda_libraries(_CT2_CUDA_LIBS)
        force_offline_hub()
        try:
            from faster_whisper import WhisperModel
        except ImportError as exc:
            raise AdapterError("runtime_missing", "CTranslate2 requires the pinned faster-whisper runtime") from exc
        compute = self.candidate.precision_for(self.manifest.device)
        self.model = WhisperModel(
            str(self.manifest.model_path),
            device="cuda" if cuda else "cpu",
            device_index=int(self.manifest.device.split(":", 1)[1]) if cuda else 0,
            compute_type=str(compute),
            cpu_threads=_threads(self.manifest),
            num_workers=1,
            local_files_only=True,
        )

    def _backend_reports_gpu(self) -> bool:
        return self.model is not None and str(self.model.model.device) == "cuda"

    def _run(self, audio: Any) -> str:
        if self.model is None:
            raise AdapterError("not_ready", "CTranslate2 model is not loaded")
        segments, _info = self.model.transcribe(
            audio,
            language=self.language,
            task="transcribe",
            beam_size=BEAM_SIZE,
            best_of=BEST_OF,
            temperature=0.0,
            vad_filter=False,
        )
        return " ".join(segment.text.strip() for segment in segments if segment.text.strip())

    def close(self) -> None:
        self.model = None
        self._inferences = 0
        gc.collect()


class GraniteCtcAdapter(BaseAdapter):
    """Granite Speech 5.0 TurboCTC (English) through native Transformers CTC support."""

    backend = "transformers:GraniteSpeechCTC"

    def __init__(self, manifest: Manifest):
        self.candidate = candidate_for(manifest, TRANSFORMERS_CTC)
        self.manifest = manifest
        self.model = None
        self.processor = None
        self.torch = None
        self._inferences = 0
        self._peak_gpu = 0
        self.metadata = AdapterMetadata(manifest.model_id, manifest.device, False, self.backend)
        require_vram_admission(manifest)
        self._load()

    def _load(self) -> None:
        force_offline_hub()
        try:
            import torch
            from transformers import AutoModelForCTC, AutoProcessor
        except ImportError as exc:
            raise AdapterError("runtime_missing", "Granite requires the pinned torch and transformers runtime") from exc
        _ensure_cuda(torch, self.manifest.device)
        dtype_name = self.candidate.precision_for(self.manifest.device)
        dtypes = {"bfloat16": torch.bfloat16, "float16": torch.float16, "float32": torch.float32}
        if dtype_name not in dtypes:
            raise AdapterError("invalid_setting", "Granite dtype must be bfloat16, float16, or float32")
        self.torch = torch
        try:
            self.processor = AutoProcessor.from_pretrained(str(self.manifest.model_path), local_files_only=True)
            model = AutoModelForCTC.from_pretrained(
                str(self.manifest.model_path), dtype=dtypes[dtype_name], local_files_only=True
            )
            # Explicit `.to(...)`: an automatic device_map could spill to CPU.
            self.model = model.to(self.manifest.device).eval()
            observed = _observe_torch_execution_device(torch, self.model, self.manifest.device)
        except AdapterError:
            raise
        except Exception as exc:
            raise AdapterError("model_load_failed", f"Granite model load failed: {exc}") from exc
        self.metadata = self._metadata(observed)

    def _metadata(self, observed: str) -> AdapterMetadata:
        self._peak_gpu = max(self._peak_gpu, _cuda_memory_bytes(self.torch, self.manifest.device))
        return AdapterMetadata(
            self.manifest.model_id,
            observed,
            resident_after_inferences(self._inferences),
            self.backend,
            host_peak_ram_bytes(),
            self._peak_gpu,
        )

    def transcribe(self, pcm: bytes) -> str:
        with _cuda_request(self.torch, self.manifest.device):
            return self._transcribe(pcm)

    def _transcribe(self, pcm: bytes) -> str:
        if self.model is None or self.processor is None or self.torch is None:
            raise AdapterError("not_ready", "Granite model is not loaded")
        try:
            import numpy as np

            audio = np.asarray(pcm_to_float32(pcm), dtype=np.float32)
            inputs = self.processor(audio, sampling_rate=16_000, return_tensors="pt")
            inputs = _move_inputs(inputs, self.model, self.manifest.device)
            with self.torch.inference_mode():
                outputs = self.model.generate(**inputs)
            text = self.processor.batch_decode(outputs, skip_special_tokens=True)[0]
            observed = _observe_torch_execution_device(self.torch, self.model, self.manifest.device)
        except AdapterError:
            raise
        except Exception as exc:
            code = "oom" if "out of memory" in str(exc).lower() else "inference_failed"
            raise AdapterError(code, f"Granite inference failed: {exc}") from exc
        self._inferences += 1
        self.metadata = self._metadata(observed)
        return " ".join(str(text).split())

    def close(self) -> None:
        self.model = None
        self.processor = None
        self._inferences = 0
        if self.torch is not None and self.manifest.device.startswith("cuda:"):
            self.torch.cuda.empty_cache()
        gc.collect()


_BACKEND_ADAPTERS: dict[str, type[BaseAdapter]] = {
    WHISPER_CPP: WhisperCppAdapter,
    CTRANSLATE2: FasterWhisperAdapter,
    TRANSFORMERS_CTC: GraniteCtcAdapter,
}


def create_candidate_adapter(manifest: Manifest) -> BaseAdapter:
    item = CANDIDATES.get(manifest.model_id)
    if item is None:
        raise AdapterError("unsupported_model", f"{manifest.model_id} is not a low-VRAM candidate")
    return _BACKEND_ADAPTERS[item.backend](manifest)
