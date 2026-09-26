"""Per-request resource telemetry, worker CPU time, and benchmark resource metrics."""

import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest import mock

ASR_ROOT = Path(__file__).parents[1]
if str(ASR_ROOT) not in sys.path:
    sys.path.insert(0, str(ASR_ROOT))
from voisu_asr import adapters as asr_adapters
from voisu_asr import service as asr_service

SPEC = importlib.util.spec_from_file_location("voisu_local_asr_benchmark_telemetry", ASR_ROOT / "benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(benchmark)

MIB = 1024 * 1024


class FakeNvmlDevice:
    """Reports whatever the simulated runtime currently holds."""

    def __init__(self):
        self.used = 0
        self.closed = False
        self.queried_pids: list[list[int]] = []

    def process_memory_bytes(self, pids):
        self.queried_pids.append(list(pids))
        return self.used

    def close(self):
        self.closed = True


def wait_for_samples(device: FakeNvmlDevice, count: int) -> None:
    deadline = time.monotonic() + 5
    start = len(device.queried_pids)
    while len(device.queried_pids) < start + count and time.monotonic() < deadline:
        time.sleep(0.005)


class RequestResourceSamplerTests(unittest.TestCase):
    def test_gpu_peak_freed_inside_the_call_is_still_reported(self):
        device = FakeNvmlDevice()
        device.used = 700 * MIB
        with mock.patch.object(asr_adapters._NvmlDevice, "open", return_value=device):
            with asr_adapters.RequestResourceSampler("cuda:0", interval_s=0.001) as sampler:
                device.used = 1300 * MIB
                wait_for_samples(device, 3)
                device.used = 700 * MIB
        self.assertEqual(sampler.peak_gpu_memory_bytes, 1300 * MIB)
        self.assertTrue(device.closed)

    def test_cpu_request_samples_host_ram_during_the_call_without_nvml(self):
        # The injected RSS peaks only between entry and exit, so it can only
        # be reported if the background sampler actually ran mid-call.
        baseline = asr_adapters.host_peak_ram_bytes()
        spike = baseline + 512 * MIB
        state = {"rss": baseline // 2, "samples": 0}

        def rss(pid):
            state["samples"] += 1
            return state["rss"]

        with mock.patch.object(asr_adapters._NvmlDevice, "open") as opened, mock.patch.object(asr_adapters, "_rss_bytes", side_effect=rss):
            with asr_adapters.RequestResourceSampler("cpu", interval_s=0.001) as sampler:
                state["rss"] = spike
                seen = state["samples"]
                deadline = time.monotonic() + 5
                while state["samples"] < seen + 3 and time.monotonic() < deadline:
                    time.sleep(0.005)
                state["rss"] = baseline // 2
        opened.assert_not_called()
        self.assertEqual(sampler.peak_gpu_memory_bytes, 0)
        self.assertEqual(sampler.peak_host_ram_bytes, spike)

    def test_child_process_memory_counts_for_subprocess_runtimes(self):
        child_code = "import time; block = bytearray(96 * 1024 * 1024); block[::4096] = b'x' * len(block[::4096]); time.sleep(0.4)"
        baseline = asr_adapters.host_peak_ram_bytes()
        with asr_adapters.RequestResourceSampler("cpu", include_children=True, interval_s=0.01) as sampler:
            subprocess.run([sys.executable, "-c", child_code], check=True)
        self.assertGreater(sampler.peak_host_ram_bytes, baseline + 64 * MIB)

    def test_process_tree_lists_live_children(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(5)"])
        try:
            self.assertIn(child.pid, asr_adapters._process_tree_pids(asr_adapters.os.getpid()))
        finally:
            child.kill()
            child.wait()


class NativeAdapterTelemetryTests(unittest.TestCase):
    def test_nemo_adapter_reports_sampled_peaks_instead_of_zero(self):
        adapter = object.__new__(asr_adapters.NemoSpeechCAdapter)
        adapter.manifest = SimpleNamespace(model_id="parakeet-tdt-0.6b-v3", device="cuda:0")
        adapter.metadata = asr_adapters.AdapterMetadata("parakeet-tdt-0.6b-v3", "cuda:0", True, "nemo-speech.cpp:c-api")
        device = FakeNvmlDevice()
        device.used = 880 * MIB

        def recognize(pcm):
            device.used = 1314 * MIB
            wait_for_samples(device, 3)
            device.used = 1200 * MIB
            return "ask not"

        with mock.patch.object(asr_adapters._NvmlDevice, "open", return_value=device), mock.patch.object(
            asr_adapters.RequestResourceSampler, "__init__", partial_init(interval_s=0.001)
        ), mock.patch.object(adapter, "_transcribe", side_effect=recognize):
            self.assertEqual(adapter.transcribe(b"\0\0"), "ask not")
        self.assertEqual(adapter.metadata.peak_gpu_memory_bytes, 1314 * MIB)
        self.assertGreater(adapter.metadata.peak_host_ram_bytes, 0)
        self.assertEqual(adapter.metadata.observed_device, "cuda:0")

    def test_moonshine_cpu_adapter_reports_host_ram(self):
        adapter = object.__new__(asr_adapters.MoonshineAdapter)
        adapter.manifest = SimpleNamespace(model_id="moonshine-streaming-medium", device="cpu")
        adapter.metadata = asr_adapters.AdapterMetadata("moonshine-streaming-medium", "cpu", True, "moonshine-voice:Transcriber")
        with mock.patch.object(adapter, "_transcribe", return_value="words"):
            adapter.transcribe(b"\0\0")
        self.assertEqual(adapter.metadata.peak_gpu_memory_bytes, 0)
        self.assertGreater(adapter.metadata.peak_host_ram_bytes, 0)


def partial_init(**overrides):
    original = asr_adapters.RequestResourceSampler.__init__

    def init(self, device, **kwargs):
        original(self, device, **{**kwargs, **overrides})

    return init


class BusyAdapter(asr_adapters.BaseAdapter):
    def __init__(self):
        self.metadata = asr_adapters.AdapterMetadata("parakeet-tdt-0.6b-v3", "cpu", True, "fake", 10, 0)

    def transcribe(self, pcm: bytes) -> str:
        deadline = time.process_time() + 0.08
        while time.process_time() < deadline:
            pass
        return "words"


def frame(value) -> bytes:
    payload = json.dumps(value).encode()
    return len(payload).to_bytes(4, "little") + payload


def read_frames(stream: io.BytesIO) -> list[dict]:
    stream.seek(0)
    frames = []
    while header := stream.read(4):
        frames.append(json.loads(stream.read(int.from_bytes(header, "little"))))
    return frames


class ServiceCpuTimeTests(unittest.TestCase):
    def test_transcript_reports_worker_cpu_time_for_the_request(self):
        requests = io.BytesIO(
            frame({"v": 1, "op": "prepare", "id": "p"})
            + frame({"v": 1, "op": "transcribe", "id": "t", "pcm_bytes": 2})
            + b"\x01\0"  # non-silent: windowing skips all-zero PCM
        )
        replies = io.BytesIO()
        with mock.patch.object(asr_service, "_prepare"):
            asr_service.serve_stream(SimpleNamespace(model_id="parakeet-tdt-0.6b-v3"), BusyAdapter(), requests, replies)
        ready, transcript = read_frames(replies)
        self.assertEqual(ready["status"], "ready")
        self.assertEqual(transcript["status"], "transcript")
        self.assertGreaterEqual(transcript["cpu_time_ms"], 60)
        self.assertEqual(transcript["peak_host_ram_bytes"], 10)


def write_venv(root: Path, home_bin: Path, size: int) -> Path:
    (root / "bin").mkdir(parents=True)
    python = root / "bin" / "python"
    python.write_bytes(b"x" * 8)
    (root / "lib.so").write_bytes(b"x" * size)
    (root / "pyvenv.cfg").write_text(f"home = {home_bin}\n", encoding="utf-8")
    return python


class InstallFootprintTests(unittest.TestCase):
    def test_shared_venv_is_counted_once_in_the_combined_total(self):
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            base = tmp / "python" / "cpython-3.12"
            (base / "bin").mkdir(parents=True)
            (base / "bin" / "python3.12").write_bytes(b"p" * 100)
            python = write_venv(tmp / "venvs" / "shared", base / "bin", 1_000)
            worker = tmp / "repo" / "worker.py"
            worker.parent.mkdir()
            worker.write_bytes(b"w" * 50)
            health = tmp / "jfk.wav"
            health.write_bytes(b"a" * 70)
            manifests = {}
            for model_id, size in (("qwen3-asr-0.6b", 300), ("qwen3-asr-1.7b", 900)):
                model_dir = tmp / "models" / model_id
                model_dir.mkdir(parents=True)
                (model_dir / "model.safetensors").write_bytes(b"m" * size)
                manifests[model_id] = {
                    "model_id": model_id,
                    "runtime": {"python": str(python), "worker": str(worker)},
                    "model_path": str(model_dir),
                    "files": [{"path": str(python)}, {"path": str(worker)}, {"path": str(health)}, {"path": str(model_dir / "model.safetensors")}],
                    "options": {"health_audio": str(health)},
                }
            footprints, summary = benchmark.install_footprints(manifests)
            venv_bytes = 8 + 1_000 + len(f"home = {base / 'bin'}\n")
            self.assertEqual(footprints["qwen3-asr-0.6b"]["model_bytes"], 300)
            self.assertEqual(footprints["qwen3-asr-0.6b"]["runtime_bytes"], venv_bytes + 100)
            self.assertEqual(footprints["qwen3-asr-1.7b"]["total_bytes"], 900 + venv_bytes + 100)
            self.assertEqual(summary["combined_unique_bytes"], 300 + 900 + venv_bytes + 100)
            self.assertEqual(summary["shared_paths"][str((tmp / "venvs" / "shared").resolve())], ["qwen3-asr-0.6b", "qwen3-asr-1.7b"])

    def test_model_inside_the_venv_is_counted_once(self):
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            base = tmp / "python"
            (base / "bin").mkdir(parents=True)
            python = write_venv(tmp / "venv", base / "bin", 1_000)
            model_dir = tmp / "venv" / "share" / "model"
            model_dir.mkdir(parents=True)
            (model_dir / "weights.bin").write_bytes(b"m" * 4_000)
            footprints, summary = benchmark.install_footprints({
                "qwen3-asr-0.6b": {
                    "runtime": {"python": str(python), "worker": str(tmp / "repo" / "worker.py")},
                    "model_path": str(model_dir),
                    "files": [{"path": str(model_dir / "weights.bin")}],
                },
            })
            footprint = footprints["qwen3-asr-0.6b"]
            venv_bytes = 8 + 1_000 + len(f"home = {base / 'bin'}\n")
            self.assertEqual(footprint["model_bytes"], 4_000)
            self.assertEqual(footprint["runtime_bytes"], venv_bytes)
            self.assertEqual(footprint["total_bytes"], 4_000 + venv_bytes)
            self.assertEqual(summary["combined_unique_bytes"], 4_000 + venv_bytes)

    def test_system_base_interpreter_is_never_walked(self):
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            python = write_venv(tmp / "venv", Path("/usr/bin"), 10)
            model = tmp / "model.gguf"
            model.write_bytes(b"g" * 20)
            with mock.patch.object(benchmark, "_path_files", wraps=benchmark._path_files) as walked:
                footprints, _ = benchmark.install_footprints({
                    "parakeet-tdt-0.6b-v3": {
                        "runtime": {"python": str(python), "worker": str(tmp / "repo" / "worker.py")},
                        "model_path": str(model),
                    },
                    # A non-venv manifest pointing at the system interpreter.
                    "moonshine-streaming-medium": {
                        "runtime": {"python": "/usr/bin/python3", "worker": str(tmp / "repo" / "worker.py")},
                        "model_path": str(model),
                    },
                })
            walked_roots = {call.args[0] for call in walked.call_args_list}
            self.assertFalse(any(root == Path("/usr") or Path("/usr") in root.parents for root in walked_roots), walked_roots)
            venv = footprints["parakeet-tdt-0.6b-v3"]
            self.assertEqual(venv["system_paths"], [str(Path("/usr").resolve())])
            self.assertEqual(venv["runtime_bytes"], 8 + 10 + len("home = /usr/bin\n"))
            system = footprints["moonshine-streaming-medium"]
            self.assertEqual(system["runtime_paths"], [])
            self.assertEqual(system["runtime_bytes"], 0)

    def test_native_library_and_standalone_model_file_are_measured(self):
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            base = tmp / "python"
            (base / "bin").mkdir(parents=True)
            python = write_venv(tmp / "venvs" / "native", base / "bin", 10)
            library = tmp / "runtimes" / "nemo" / "lib" / "libnemo.so"
            library.parent.mkdir(parents=True)
            library.write_bytes(b"l" * 400)
            model = tmp / "models" / "parakeet" / "parakeet.gguf"
            model.parent.mkdir(parents=True)
            model.write_bytes(b"g" * 700)
            (model.parent / "unrelated-download.tmp").write_bytes(b"u" * 5_000)
            footprints, _ = benchmark.install_footprints({"parakeet-tdt-0.6b-v3": {
                "runtime": {"python": str(python), "worker": str(tmp / "worker.py")},
                "model_path": str(model),
                "files": [{"path": str(model)}, {"path": str(library)}],
                "options": {"library": str(library)},
            }})
            footprint = footprints["parakeet-tdt-0.6b-v3"]
            venv_bytes = 8 + 10 + len(f"home = {base / 'bin'}\n")
            self.assertEqual(footprint["model_bytes"], 700)
            self.assertIn(str(library.parent.resolve()), footprint["runtime_paths"])
            self.assertEqual(footprint["runtime_bytes"], venv_bytes + 400)

    def test_manifest_without_runtime_paths_has_no_footprint(self):
        footprints, summary = benchmark.install_footprints({"qwen3-asr-0.6b": {"model_id": "qwen3-asr-0.6b"}})
        self.assertIsNone(footprints["qwen3-asr-0.6b"])
        self.assertEqual(summary["combined_unique_bytes"], 0)


class ReportResourceMetricTests(unittest.TestCase):
    def test_report_has_cpu_time_rtf_and_vram_by_duration_profile(self):
        rows = [
            {"raw_response": {"status": "transcript"}, "phase": "cold", "resident": True, "duration_profile": "short", "inference_ms": 50, "real_time_factor": 0.01, "cpu_time_ms": 60, "peak_gpu_memory_bytes": 880 * MIB, "peak_host_ram_bytes": 250 * MIB},
            {"raw_response": {"status": "transcript"}, "phase": "warm", "resident": True, "duration_profile": "long", "inference_ms": 300, "real_time_factor": 0.005, "cpu_time_ms": 320, "peak_gpu_memory_bytes": 1314 * MIB, "peak_host_ram_bytes": 318 * MIB},
            {"raw_response": {"status": "error"}, "phase": "warm", "duration_profile": "long", "cpu_time_ms": 999},
        ]
        corpus = {"records": []}
        report = benchmark._report_model("parakeet-tdt-0.6b-v3", rows, corpus, None, {}, True)
        self.assertEqual(report["timings"]["cpu_time_ms"]["sample_count"], 2)
        self.assertEqual(report["timings"]["cpu_time_ms"]["p95_ms"], 320)
        self.assertEqual(report["timings"]["real_time_factor"]["p50"], 0.005)
        self.assertEqual(report["resources"]["peak_gpu_memory_bytes_by_duration_profile"], {"long": 1314 * MIB, "short": 880 * MIB})
        self.assertEqual(report["resources"]["peak_host_ram_bytes_by_duration_profile"]["long"], 318 * MIB)

    def test_zero_gpu_peak_on_a_cuda_row_is_unknown_not_measured(self):
        rows = [
            {"raw_response": {"status": "transcript"}, "phase": "cold", "requested_device": "cuda:0", "observed_device": "cuda:0", "duration_profile": "short", "peak_gpu_memory_bytes": 0, "peak_host_ram_bytes": 0},
            {"raw_response": {"status": "transcript"}, "phase": "warm", "requested_device": "cuda:0", "observed_device": "cuda:0", "duration_profile": "long", "peak_gpu_memory_bytes": 1314 * MIB, "peak_host_ram_bytes": 318 * MIB},
        ]
        report = benchmark._report_model("parakeet-tdt-0.6b-v3", rows, {"records": []}, None, {}, True)
        resources = report["resources"]
        self.assertEqual(resources["peak_gpu_memory_bytes"], 1314 * MIB)
        self.assertEqual(resources["peak_gpu_memory_bytes_by_duration_profile"], {"long": 1314 * MIB, "short": None})
        self.assertEqual(resources["peak_host_ram_bytes_by_duration_profile"], {"long": 318 * MIB, "short": None})
        unknown = benchmark._report_model("parakeet-tdt-0.6b-v3", rows[:1], {"records": []}, None, {}, True)["resources"]
        self.assertIsNone(unknown["peak_gpu_memory_bytes"])
        self.assertIsNone(unknown["peak_host_ram_bytes"])

    def test_zero_gpu_peak_on_a_cpu_row_is_a_measured_zero(self):
        row = {"requested_device": "cpu", "observed_device": "cpu", "peak_gpu_memory_bytes": 0}
        self.assertEqual(benchmark.measured_peak(row, "peak_gpu_memory_bytes"), 0.0)
        self.assertIsNone(benchmark.measured_peak({**row, "requested_device": "cuda:0"}, "peak_gpu_memory_bytes"))

    def test_real_time_factor_needs_positive_audio_duration(self):
        self.assertEqual(benchmark.real_time_factor(500, 10_000), 0.05)
        self.assertIsNone(benchmark.real_time_factor(500, 0))
        self.assertIsNone(benchmark.real_time_factor(None, 10_000))


if __name__ == "__main__":
    unittest.main()
