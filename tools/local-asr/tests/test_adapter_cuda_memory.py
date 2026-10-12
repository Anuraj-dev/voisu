"""Per-request CUDA memory release and peak telemetry, with a fake torch (no GPU)."""

import contextlib
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest import mock
import weakref

import numpy

ASR_ROOT = Path(__file__).parents[1]
if str(ASR_ROOT) not in sys.path:
    sys.path.insert(0, str(ASR_ROOT))
from voisu_asr import adapters as asr_adapters

GIB = 1024**3
WEIGHTS = 1 * GIB
PCM = b"\0\0" * 1600


class FakeTensor:
    """Stands in for a per-request tensor; weakly referenced to prove it was dropped."""

    shape = (1, 4)

    def keys(self):
        return ["input_ids"]

    def to(self, *args):
        return self

    def __getitem__(self, key):
        return self if key == "input_ids" else FakeTensor()


class FakeCuda:
    def __init__(self):
        self.allocated = WEIGHTS
        self.peak = WEIGHTS
        self.calls: list[str] = []
        self.live_at_release: list[bool] = []
        self.watched: list[weakref.ref] = []

    def reset_peak_memory_stats(self, index=None):
        self.calls.append("reset_peak")
        self.peak = self.allocated

    def max_memory_allocated(self, index=None):
        return self.peak

    def memory_allocated(self, index=None):
        return self.allocated

    def empty_cache(self):
        self.calls.append("empty_cache")
        self.live_at_release.append(any(ref() is not None for ref in self.watched))


class FakeTorch:
    def __init__(self):
        self.cuda = FakeCuda()

    def inference_mode(self):
        return contextlib.nullcontext()


class FakeProcessor:
    def __init__(self, cuda):
        self.cuda = cuda

    def apply_transcription_request(self, **kwargs):
        inputs = FakeTensor()
        self.cuda.watched.append(weakref.ref(inputs))
        return inputs

    def decode(self, ids, **kwargs):
        return "hello world"


class FakeModel:
    dtype = None

    def __init__(self, cuda, request_peaks, fail=False):
        self.cuda = cuda
        self.request_peaks = list(request_peaks)
        self.fail = fail

    def parameters(self):
        return [SimpleNamespace(device="cuda:0")]

    def generate(self, **kwargs):
        activations = FakeTensor()
        self.cuda.watched.append(weakref.ref(activations))
        self.cuda.peak = max(self.cuda.peak, self.request_peaks.pop(0))
        if self.fail:
            raise RuntimeError("synthetic inference failure")
        return activations


class Raiser:
    """Any use raises, so every adapter fails inside its inference path."""

    def __getattr__(self, name):
        return self

    def __call__(self, *args, **kwargs):
        raise RuntimeError("synthetic inference failure")


def cuda_manifest(model_id: str) -> SimpleNamespace:
    return SimpleNamespace(model_id=model_id, device="cuda:0", language="en", options={})


def bare_adapter(cls, model_id: str, torch, **attrs):
    adapter = object.__new__(cls)
    adapter.manifest = cuda_manifest(model_id)
    adapter.torch = torch
    adapter._inferences = 0
    adapter._peak_host_ram_bytes = 0
    adapter._peak_gpu_memory_bytes = 0
    adapter.metadata = asr_adapters.AdapterMetadata(model_id, "cuda:0", False, "fake")
    for name, value in attrs.items():
        setattr(adapter, name, value)
    return adapter


def qwen_adapter(torch, request_peaks, fail=False):
    return bare_adapter(
        asr_adapters.QwenTransformersAdapter,
        "qwen3-asr-0.6b",
        torch,
        model=FakeModel(torch.cuda, request_peaks, fail),
        processor=FakeProcessor(torch.cuda),
    )


@mock.patch.object(asr_adapters, "_nvml_process_gpu_memory_bytes", return_value=None)
class CudaRequestMemoryTests(unittest.TestCase):
    def test_success_releases_cache_after_request_tensors_are_dropped(self, _nvml):
        torch = FakeTorch()
        adapter = qwen_adapter(torch, [2 * GIB])

        self.assertEqual(adapter.transcribe(PCM), "hello world")

        self.assertEqual(torch.cuda.calls, ["reset_peak", "empty_cache"])
        self.assertEqual(torch.cuda.live_at_release, [False])

    def test_inference_exception_still_releases_cache_and_request_tensors(self, _nvml):
        torch = FakeTorch()
        adapter = qwen_adapter(torch, [2 * GIB], fail=True)

        with self.assertRaises(asr_adapters.AdapterError) as raised:
            adapter.transcribe(PCM)

        self.assertEqual(raised.exception.code, "inference_failed")
        self.assertEqual(torch.cuda.calls, ["reset_peak", "empty_cache"])
        self.assertEqual(torch.cuda.live_at_release, [False])

    def test_reported_peak_belongs_to_each_request(self, _nvml):
        torch = FakeTorch()
        adapter = qwen_adapter(torch, [6 * GIB, 2 * GIB])

        adapter.transcribe(PCM)
        self.assertEqual(adapter.metadata.peak_gpu_memory_bytes, 6 * GIB)
        adapter.transcribe(PCM)
        self.assertEqual(adapter.metadata.peak_gpu_memory_bytes, 2 * GIB)

    def test_failed_peak_reset_reports_unknown_instead_of_a_stale_peak(self, _nvml):
        torch = FakeTorch()
        adapter = qwen_adapter(torch, [6 * GIB, 2 * GIB])
        adapter.transcribe(PCM)
        self.assertEqual(adapter.metadata.peak_gpu_memory_bytes, 6 * GIB)

        def broken_reset(index=None):
            raise RuntimeError("synthetic reset failure")

        torch.cuda.reset_peak_memory_stats = broken_reset
        self.assertEqual(adapter.transcribe(PCM), "hello world")

        self.assertEqual(adapter.metadata.peak_gpu_memory_bytes, 0)
        self.assertEqual(torch.cuda.calls[-1], "empty_cache")

    def test_every_torch_adapter_releases_cache_when_inference_fails(self, _nvml):
        cases = [
            (asr_adapters.QwenTransformersAdapter, "qwen3-asr-0.6b", {}),
            (asr_adapters.FunAsrAdapter, "fun-asr-nano-2512", {}),
            (asr_adapters.FunAsrHfTransformersAdapter, "fun-asr-nano-2512", {}),
            (asr_adapters.CohereTransformersAdapter, "cohere-transcribe-03-2026", {"numpy": numpy}),
        ]
        for cls, model_id, extra in cases:
            with self.subTest(adapter=cls.__name__):
                torch = FakeTorch()
                adapter = bare_adapter(cls, model_id, torch, model=Raiser(), processor=Raiser(), **extra)

                with self.assertRaises(asr_adapters.AdapterError):
                    adapter.transcribe(PCM)

                self.assertEqual(torch.cuda.calls, ["reset_peak", "empty_cache"])


if __name__ == "__main__":
    unittest.main()
