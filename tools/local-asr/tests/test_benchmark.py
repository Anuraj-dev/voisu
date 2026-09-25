import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
import wave


MODULE_PATH = Path(__file__).parents[1] / "benchmark.py"
SPEC = importlib.util.spec_from_file_location("voisu_local_asr_benchmark", MODULE_PATH)
benchmark = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(benchmark)

ASR_ROOT = Path(__file__).parents[1]
if str(ASR_ROOT) not in sys.path:
    sys.path.insert(0, str(ASR_ROOT))
from voisu_asr import adapters as asr_adapters
from voisu_asr import manifest as asr_manifest
from voisu_asr import service as asr_service


def make_wav(path: Path, frames: int = 1600) -> None:
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(16_000)
        handle.writeframes(b"\0\0" * frames)


def make_corpus(tmp: Path):
    audio_dir = tmp / "audio"
    audio_dir.mkdir()
    make_wav(audio_dir / "english.wav")
    make_wav(audio_dir / "hinglish.wav", 2400)
    make_wav(audio_dir / "silence.wav", 800)
    refs = {
        "english.wav": {"language": "english", "reference": "ship the release", "consent_id": "c-1", "tags": ["commands"], "critical_tokens": ["release"]},
        "hinglish.wav": {"language": "hinglish", "reference": "kal deploy karna", "consent_id": "c-1"},
        "silence.wav": {"language": "negative", "reference": "", "consent_id": "c-1"},
    }
    refs_path = tmp / "refs.json"
    refs_path.write_text(json.dumps(refs), encoding="utf-8")
    consent_path = tmp / "consent.json"
    consent_path.write_text(json.dumps({"consent_id": "c-1"}), encoding="utf-8")
    inventory = benchmark.inventory(audio_dir, refs_path, consent_path, "local-pilot-v1")
    frozen = benchmark.freeze(inventory, "test-seed", 0.34)
    return inventory, frozen


class BenchmarkTests(unittest.TestCase):
    def test_wer_reports_substitutions_deletions_and_insertions(self):
        result = benchmark.wer(["a", "b", "c"], ["a", "x", "c", "d"])
        self.assertEqual(result["substitutions"], 1)
        self.assertEqual(result["deletions"], 0)
        self.assertEqual(result["insertions"], 1)
        self.assertEqual(result["distance"], 2)

    def test_inventory_and_freeze_are_hash_checked_and_split_deterministically(self):
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            inventory, frozen = make_corpus(tmp)
            benchmark.validate_inventory(inventory)
            benchmark.validate_frozen(frozen)
            again = benchmark.freeze(inventory, "test-seed", 0.34)
            self.assertEqual(frozen["manifest_sha256"], again["manifest_sha256"])
            self.assertEqual([row["split"] for row in frozen["records"]], [row["split"] for row in again["records"]])
            frozen["records"][0]["reference"] = "changed"
            with self.assertRaises(benchmark.BenchmarkError):
                benchmark.validate_frozen(frozen)

    def test_score_keeps_raw_and_delivered_separate_and_counts_negative_insertions(self):
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            _, corpus = make_corpus(tmp)
            model = "qwen3-asr-0.6b"
            rows = []
            for record in corpus["records"]:
                if record["language"] == "english":
                    raw, delivered = "ship the release extra", "ship the release"
                    status = "transcript"
                elif record["language"] == "hinglish":
                    raw, delivered = "kal deploy karna", "kal deploy karna"
                    status = "transcript"
                else:
                    raw, delivered = "noise", "noise"
                    status = "transcript"
                rows.append({
                    "schema_version": 1,
                    "run_id": "fixture-run",
                    "record_id": record["record_id"],
                    "audio_sha256": record["audio_sha256"],
                    "corpus_manifest_sha256": corpus["manifest_sha256"],
                    "language": record["language"],
                    "split": record["split"],
                    "model_id": model,
                    "protected_execution": False,
                    "execution_seam": "fixture",
                    "evidence_class": "fixture",
                    "raw_response": {"status": status, "model_id": model},
                    "raw_text": raw,
                    "delivered_text": delivered,
                })
            evidence = {"schema_version": 1, "kind": "replay_evidence", "run_id": "fixture-run", "corpus_manifest_sha256": corpus["manifest_sha256"], "records": rows}
            evidence["evidence_sha256"] = benchmark.content_hash(evidence, "evidence_sha256")
            report = benchmark.score(corpus, evidence, [model], None, allow_fixture=True)
            model_report = report["models"][0]
            self.assertEqual(model_report["languages"]["english"]["raw_strict"]["insertions"], 1)
            self.assertEqual(model_report["languages"]["english"]["delivered_strict"]["wer"], 0)
            self.assertEqual(model_report["languages"]["negative"]["raw_strict"]["insertions"], 1)
            self.assertFalse(report["quality_evidence"])

    def test_stale_evidence_is_rejected_before_scoring(self):
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            _, corpus = make_corpus(tmp)
            evidence = {"schema_version": 1, "kind": "replay_evidence", "run_id": "stale", "corpus_manifest_sha256": "0" * 64, "records": [], "evidence_sha256": ""}
            evidence["evidence_sha256"] = benchmark.content_hash(evidence, "evidence_sha256")
            with self.assertRaises(benchmark.BenchmarkError):
                benchmark.score(corpus, evidence, ["qwen3-asr-0.6b"], None, allow_fixture=True)

    def test_recommendation_does_not_invent_unmeasured_labels(self):
        report = {"models": [{"model_id": "qwen3-asr-0.6b", "status": "measured", "quality_evidence": True, "identity": {"host_profile": "rtx-5050", "duration_profile": "short"}, "languages": {"english": {"raw_strict": {"sample_count": 2, "wer": 0.1}}}, "timings": {"warm_inference_ms": {"p95_ms": None}}, "resources": {"peak_host_ram_bytes": None}}]}
        result = benchmark.recommend(report, "english", "short", "rtx-5050")
        self.assertEqual(result["labels"]["highest_tested_accuracy"], "qwen3-asr-0.6b")
        self.assertIsNone(result["labels"]["fastest_warm"])
        self.assertIsNone(result["labels"]["lowest_measured_memory"])

    def test_replay_command_uses_only_rust_protected_seam(self):
        command = benchmark.build_replay_command("voisu", Path("/tmp/model.json"), Path("/tmp/audio.wav"))
        self.assertEqual(command, ["voisu", "local-dev", "replay", "--json", "--manifest", "/tmp/model.json", "--audio", "/tmp/audio.wav"])

    def test_worker_entry_is_worker_py_and_cache_dir_is_allowed(self):
        self.assertEqual(asr_service.WORKER_ENTRY.name, "worker.py")
        self.assertNotEqual(Path(asr_service.__file__).name, "worker.py")
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            python = tmp / "python"
            python.write_text("#!/bin/sh\n", encoding="utf-8")
            python.chmod(0o755)
            worker = tmp / "worker.py"
            worker.write_text("pass\n", encoding="utf-8")
            model = tmp / "qwen-model"
            model.mkdir()
            qwen_files = []
            for name in asr_manifest.QWEN_REQUIRED_FILES:
                path = model / name
                path.write_text(name, encoding="utf-8")
                qwen_files.append(path)
            health = tmp / "health.wav"
            make_wav(health)
            cache = tmp / "unpinned-cache"

            def pin(path: Path) -> dict:
                data = path.read_bytes()
                return {
                    "path": str(path),
                    "sha256": hashlib.sha256(data).hexdigest(),
                    "bytes": len(data),
                }

            manifest_path = tmp / "manifest.json"
            manifest_path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "qwen3-asr-0.6b",
                        "device": "cpu",
                        "language": "en",
                        "runtime": {
                            "python": str(python),
                            "worker": str(worker),
                            "revision": "test-runtime",
                        },
                        "model_path": str(model),
                        "files": [pin(python), pin(worker), pin(health), *[pin(path) for path in qwen_files]],
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "cache_dir": str(cache),
                        },
                        "limits": {
                            "memory_bytes": 1024,
                            "tasks": 4,
                            "cpu_cores": 1,
                            "cache_bytes": 1024,
                            "vram_bytes": 0,
                        },
                    }
                ),
                encoding="utf-8",
            )
            loaded = asr_manifest.load_manifest(manifest_path)
            self.assertEqual(loaded.options["cache_dir"], str(cache))
            self.assertNotIn(cache, {item.path for item in loaded.artifacts})

    def test_observe_execution_device_rejects_silent_cpu_spill(self):
        self.assertEqual(
            asr_adapters.observe_execution_device(
                "cpu",
                maps_text="",
                gpu_memory_bytes=None,
                cuda_device_index=None,
            ),
            "cpu",
        )
        self.assertEqual(
            asr_adapters.observe_execution_device(
                "cuda:0",
                maps_text="/usr/lib/libggml-cuda.so.0",
                gpu_memory_bytes=1024,
                cuda_device_index=0,
            ),
            "cuda:0",
        )
        with self.assertRaises(asr_adapters.AdapterError) as missing_maps:
            asr_adapters.observe_execution_device(
                "cuda:0",
                maps_text="libggml-cpu.so",
                gpu_memory_bytes=0,
                cuda_device_index=None,
            )
        self.assertEqual(missing_maps.exception.code, "backend_device_mismatch")
        with self.assertRaises(asr_adapters.AdapterError) as cpu_spill:
            asr_adapters.observe_execution_device(
                "cuda:0",
                maps_text="libggml-cuda.so.0",
                gpu_memory_bytes=0,
                cuda_device_index=None,
            )
        self.assertEqual(cpu_spill.exception.code, "backend_device_mismatch")
        self.assertIn("CPU spill", str(cpu_spill.exception))
        with self.assertRaises(asr_adapters.AdapterError) as context_only:
            asr_adapters.observe_execution_device(
                "cuda:0",
                maps_text="libcuda.so.1",
                gpu_memory_bytes=0,
                cuda_device_index=0,
            )
        self.assertEqual(context_only.exception.code, "backend_device_mismatch")
        with self.assertRaises(asr_adapters.AdapterError) as no_backend:
            asr_adapters.observe_execution_device(
                "cuda:0",
                maps_text="libggml-cuda.so.0",
                gpu_memory_bytes=1024,
                cuda_device_index=0,
                after_inference=True,
                backend_reports_gpu=False,
            )
        self.assertEqual(no_backend.exception.code, "backend_device_mismatch")
        self.assertEqual(
            asr_adapters.observe_execution_device(
                "cuda:0",
                maps_text="libggml-cuda.so.0",
                gpu_memory_bytes=1024,
                cuda_device_index=0,
                after_inference=True,
                backend_reports_gpu=True,
            ),
            "cuda:0",
        )

    def test_resident_is_false_until_the_second_inference(self):
        self.assertFalse(asr_adapters.resident_after_inferences(0))
        self.assertFalse(asr_adapters.resident_after_inferences(1))
        self.assertTrue(asr_adapters.resident_after_inferences(2))
        self.assertTrue(asr_adapters.resident_after_inferences(3))

    def test_qwen_language_dtype_offline_and_vram_admission(self):
        self.assertEqual(asr_adapters.resolve_qwen_language("en"), "English")
        self.assertEqual(asr_adapters.resolve_qwen_language("hi"), "Hindi")
        self.assertIsNone(asr_adapters.resolve_qwen_language("auto"))
        with self.assertRaises(asr_adapters.AdapterError) as unsupported:
            asr_adapters.resolve_qwen_language("fr")
        self.assertEqual(unsupported.exception.code, "unsupported_language")
        self.assertEqual(asr_adapters.qwen_dtype_name("cuda:0", {}), "bfloat16")
        self.assertEqual(asr_adapters.qwen_dtype_name("cpu", {}), "float32")
        with self.assertRaises(asr_adapters.AdapterError) as dtype:
            asr_adapters.qwen_dtype_name("cuda:0", {"dtype": "float8"})
        self.assertEqual(dtype.exception.code, "invalid_setting")
        asr_adapters.force_offline_hub()
        self.assertEqual(os.environ.get("HF_HUB_OFFLINE"), "1")
        self.assertEqual(os.environ.get("TRANSFORMERS_OFFLINE"), "1")
        total = asr_adapters.RTX_5050_TOTAL_VRAM_BYTES
        budget = asr_adapters.vram_admission_bytes(total)
        reserve = asr_adapters.desktop_reserve_bytes(total)
        self.assertEqual(budget, total * 75 // 100)
        self.assertEqual(budget + reserve, total)
        self.assertTrue(asr_adapters.gpu_profile_fits(budget, total, free_vram_bytes=total))
        self.assertFalse(asr_adapters.gpu_profile_fits(budget + 1, total))
        self.assertFalse(asr_adapters.gpu_profile_fits(0, total))
        over_free = asr_adapters.gpu_profile_fits(
            budget,
            total,
            free_vram_bytes=reserve,
        )
        self.assertFalse(over_free)
        # Host-measured peaks from the 2026-09-22 RTX 5050 Transformers run.
        self.assertTrue(asr_adapters.gpu_profile_fits(1_891_631_104, total))
        self.assertTrue(asr_adapters.gpu_profile_fits(4_399_824_896, total))
        qwen_ok = asr_manifest.Manifest(
            version=1,
            model_id="qwen3-asr-0.6b",
            device="cuda:0",
            language="en",
            python=Path("/bin/true"),
            worker=Path("/tmp/worker.py"),
            revision="test",
            requirements_lock=None,
            model_path=Path("/tmp/qwen"),
            artifacts=(),
            options={},
            limits={"vram_bytes": 1_891_631_104},
            source_path=Path("/tmp/qwen.json"),
        )
        asr_adapters.require_qwen_vram_admission(qwen_ok, total, total)
        with self.assertRaises(asr_adapters.AdapterError) as blocked:
            asr_adapters.require_qwen_vram_admission(qwen_ok, total, reserve)
        self.assertEqual(blocked.exception.code, "vram_admission")
        self.assertIs(asr_adapters.ADAPTER_LOADERS["qwen3-asr-0.6b"], asr_adapters.QwenTransformersAdapter)
        self.assertIs(asr_adapters.ADAPTER_LOADERS["qwen3-asr-1.7b"], asr_adapters.QwenTransformersAdapter)

    def test_host_peak_ram_uses_rusage_maxrss_not_current_vmrss(self):
        import resource

        peak = asr_adapters.host_peak_ram_bytes()
        rusage = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024
        self.assertGreater(peak, 0)
        self.assertEqual(peak, rusage)
        source = Path(asr_adapters.__file__).read_text(encoding="utf-8")
        self.assertIn("ru_maxrss", source)
        self.assertIn("host_peak_ram_bytes", source)
        record = source.split("def _record_peaks", 1)[1].split("def _metadata", 1)[0]
        self.assertIn("host_peak_ram_bytes()", record)
        self.assertNotIn("host_rss_bytes()", record)

    def test_qwen_adapter_source_stays_transformers_offline_and_local(self):
        source = Path(asr_adapters.__file__).read_text(encoding="utf-8")
        self.assertNotIn("0.0.0.0", source)
        self.assertNotIn("vllm", source.lower())
        self.assertNotIn('device_map="auto"', source)
        self.assertNotIn("device_map='auto'", source)
        self.assertIn("local_files_only", source)
        self.assertIn("HF_HUB_OFFLINE", source)
        self.assertIn("QwenTransformersAdapter", source)

    def test_parakeet_rejects_hindi_and_auto(self):
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            python = tmp / "python"
            python.write_text("#!/bin/sh\n", encoding="utf-8")
            python.chmod(0o755)
            worker = tmp / "worker.py"
            worker.write_text("pass\n", encoding="utf-8")
            model = tmp / "model.bin"
            model.write_bytes(b"model")
            library = tmp / "libnemo_speech_asr_c.so.1"
            library.write_bytes(b"lib")
            health = tmp / "health.wav"
            make_wav(health)

            def pin(path: Path) -> dict:
                data = path.read_bytes()
                return {
                    "path": str(path),
                    "sha256": hashlib.sha256(data).hexdigest(),
                    "bytes": len(data),
                }

            for language in ("hi", "auto"):
                manifest_path = tmp / f"parakeet-{language}.json"
                manifest_path.write_text(
                    json.dumps(
                        {
                            "version": 1,
                            "model_id": "parakeet-tdt-0.6b-v3",
                            "device": "cpu",
                            "language": language,
                            "runtime": {
                                "python": str(python),
                                "worker": str(worker),
                                "revision": "test-runtime",
                            },
                            "model_path": str(model),
                            "files": [pin(python), pin(worker), pin(model), pin(library), pin(health)],
                            "options": {
                                "health_audio": str(health),
                                "health_expected": "hello",
                                "library": str(library),
                            },
                            "limits": {
                                "memory_bytes": 1024,
                                "tasks": 4,
                                "cpu_cores": 1,
                                "cache_bytes": 1024,
                                "vram_bytes": 0,
                            },
                        }
                    ),
                    encoding="utf-8",
                )
                with self.assertRaises(asr_manifest.ManifestError):
                    asr_manifest.load_manifest(manifest_path)

            manifest_path = tmp / "parakeet-hi-code.json"
            manifest_path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "parakeet-tdt-0.6b-v3",
                        "device": "cpu",
                        "language": "en",
                        "runtime": {
                            "python": str(python),
                            "worker": str(worker),
                            "revision": "test-runtime",
                        },
                        "model_path": str(model),
                        "files": [pin(python), pin(worker), pin(model), pin(library), pin(health)],
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "library": str(library),
                            "language_code": "hi-IN",
                        },
                        "limits": {
                            "memory_bytes": 1024,
                            "tasks": 4,
                            "cpu_cores": 1,
                            "cache_bytes": 1024,
                            "vram_bytes": 0,
                        },
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(asr_manifest.ManifestError):
                asr_manifest.load_manifest(manifest_path)

    def test_parakeet_language_code_cannot_override_english(self):
        self.assertEqual(
            asr_adapters.resolve_nemo_language_code("parakeet-tdt-0.6b-v3", "en", {}),
            "en-US",
        )
        self.assertEqual(
            asr_adapters.resolve_nemo_language_code(
                "parakeet-tdt-0.6b-v3", "en", {"language_code": "en"}
            ),
            "en-US",
        )
        with self.assertRaises(asr_adapters.AdapterError) as hindi:
            asr_adapters.resolve_nemo_language_code(
                "parakeet-tdt-0.6b-v3", "en", {"language_code": "hi-IN"}
            )
        self.assertEqual(hindi.exception.code, "unsupported_language")
        with self.assertRaises(asr_adapters.AdapterError):
            asr_adapters.resolve_nemo_language_code("parakeet-tdt-0.6b-v3", "hi", {})
        self.assertEqual(
            asr_adapters.resolve_nemo_language_code(
                "nemotron-3.5-0.6b", "hi", {"language_code": "hi-IN"}
            ),
            "hi-IN",
        )

    def _write_exec(self, path: Path, payload: bytes = b"x") -> None:
        path.write_bytes(payload)
        path.chmod(0o755)

    def _pin(self, path: Path) -> dict:
        data = path.read_bytes()
        return {"path": str(path), "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}

    def _base_runtime(self, tmp: Path) -> tuple[Path, Path, Path, Path]:
        python = tmp / "python"
        self._write_exec(python, b"#!/bin/sh\n")
        worker = tmp / "worker.py"
        worker.write_text("pass\n", encoding="utf-8")
        model = tmp / "model.bin"
        model.write_bytes(b"model")
        health = tmp / "health.wav"
        make_wav(health)
        return python, worker, model, health

    def test_fun_asr_rejects_mlt_and_requires_nano_checkpoint(self):
        self.assertIsNone(
            asr_adapters.fun_asr_identity_error(
                "/models/Fun-ASR-Nano-2512",
                {"checkpoint": "Fun-ASR-Nano-2512"},
            )
        )
        self.assertIn(
            "Fun-ASR-MLT-Nano",
            asr_adapters.fun_asr_identity_error(
                "/models/Fun-ASR-MLT-Nano-2512",
                {"checkpoint": "Fun-ASR-Nano-2512"},
            ),
        )
        self.assertIn(
            "Fun-ASR-Nano-2512",
            asr_adapters.fun_asr_identity_error("/models/Fun-ASR-Nano-2512", {}),
        )
        self.assertEqual(
            asr_adapters.parse_funasr_llama_cli_transcript(
                "ggml_cuda_init: no GPU\nAnd so my fellow Americans\n"
            ),
            "And so my fellow Americans",
        )
        self.assertIn("fun-asr-nano-2512", asr_adapters.ADAPTER_LOADERS)
        self.assertIs(asr_adapters.ADAPTER_LOADERS["fun-asr-nano-2512"], asr_adapters.create_fun_asr_adapter)

        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            python, worker, model, health = self._base_runtime(tmp)
            encoder = tmp / "funasr-encoder-f16.gguf"
            decoder = tmp / "qwen3-0.6b-q8_0.gguf"
            cli = tmp / "llama-funasr-cli"
            encoder.write_bytes(b"enc")
            decoder.write_bytes(b"dec")
            self._write_exec(cli, b"cli")
            mlt = tmp / "Fun-ASR-MLT-Nano-2512"
            mlt.mkdir()
            (mlt / "model.bin").write_bytes(b"mlt")
            pins = [self._pin(python), self._pin(worker), self._pin(model), self._pin(health), self._pin(encoder), self._pin(decoder), self._pin(cli)]
            for language in ("hi", "auto"):
                path = tmp / f"fun-{language}.json"
                path.write_text(
                    json.dumps(
                        {
                            "version": 1,
                            "model_id": "fun-asr-nano-2512",
                            "device": "cpu",
                            "language": language,
                            "runtime": {"python": str(python), "worker": str(worker), "revision": "test"},
                            "model_path": str(model),
                            "files": pins,
                            "options": {
                                "health_audio": str(health),
                                "health_expected": "hello",
                                "checkpoint": "Fun-ASR-Nano-2512",
                                "encoder": str(encoder),
                                "decoder": str(decoder),
                                "llama_cli": str(cli),
                            },
                            "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                        }
                    ),
                    encoding="utf-8",
                )
                with self.assertRaises(asr_manifest.ManifestError):
                    asr_manifest.load_manifest(path)

            mlt_path = tmp / "fun-mlt.json"
            mlt_path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "fun-asr-nano-2512",
                        "device": "cpu",
                        "language": "en",
                        "runtime": {"python": str(python), "worker": str(worker), "revision": "test"},
                        "model_path": str(mlt),
                        "files": pins,
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "checkpoint": "Fun-ASR-Nano-2512",
                            "encoder": str(encoder),
                            "decoder": str(decoder),
                            "llama_cli": str(cli),
                        },
                        "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(asr_manifest.ManifestError):
                asr_manifest.load_manifest(mlt_path)

            native_stub = tmp / "fun-native-stub.json"
            native_stub.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "fun-asr-nano-2512",
                        "device": "cpu",
                        "language": "en",
                        "runtime": {"python": str(python), "worker": str(worker), "revision": asr_adapters.FUN_ASR_LLAMA_RUNTIME},
                        "model_path": str(model),
                        "files": pins,
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "checkpoint": "Fun-ASR-Nano-2512",
                            "encoder": str(encoder),
                            "decoder": str(decoder),
                            "llama_cli": str(cli),
                        },
                        "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(asr_manifest.ManifestError) as published:
                asr_manifest.load_manifest(native_stub)
            self.assertIn("Fun-ASR-Nano", str(published.exception))
            unpinned = tmp / "fun-unpinned.json"
            unpinned.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "fun-asr-nano-2512",
                        "device": "cpu",
                        "language": "en",
                        "runtime": {"python": str(python), "worker": str(worker), "revision": asr_adapters.FUN_ASR_LLAMA_RUNTIME},
                        "model_path": str(model),
                        "files": [self._pin(python), self._pin(worker), self._pin(model), self._pin(health)],
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "checkpoint": "Fun-ASR-Nano-2512",
                            "encoder": str(encoder),
                            "decoder": str(decoder),
                            "llama_cli": str(cli),
                        },
                        "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(asr_manifest.ManifestError):
                asr_manifest.load_manifest(unpinned)
            remote = tmp / "model.py"
            remote.write_text("pass\n", encoding="utf-8")
            ok_path = tmp / "fun-ok.json"
            ok_path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "fun-asr-nano-2512",
                        "device": "cpu",
                        "language": "en",
                        "runtime": {"python": str(python), "worker": str(worker), "revision": "funasr-python"},
                        "model_path": str(model),
                        "files": [self._pin(python), self._pin(worker), self._pin(model), self._pin(health), self._pin(remote)],
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "checkpoint": "Fun-ASR-Nano-2512",
                            "remote_code": str(remote),
                        },
                        "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                    }
                ),
                encoding="utf-8",
            )
            loaded = asr_manifest.load_manifest(ok_path)
            self.assertEqual(loaded.options["checkpoint"], "Fun-ASR-Nano-2512")
            self.assertEqual(asr_adapters.FUN_ASR_PYTHON_ENGLISH, "英文")
            cuda_native = asr_manifest.Manifest(
                version=1,
                model_id="fun-asr-nano-2512",
                device="cuda:0",
                language="en",
                python=python,
                worker=worker,
                revision="test",
                requirements_lock=None,
                model_path=model,
                artifacts=(),
                options={"checkpoint": "Fun-ASR-Nano-2512", "llama_cli": str(cli)},
                limits={},
                source_path=ok_path,
            )
            with self.assertRaises(asr_adapters.AdapterError) as cuda_cli:
                asr_adapters.FunAsrLlamaCppAdapter(cuda_native)
            self.assertEqual(cuda_cli.exception.code, "native_cuda_unsuitable")
            with self.assertRaises(asr_adapters.AdapterError) as factory_cuda:
                asr_adapters.create_fun_asr_adapter(cuda_native)
            self.assertNotEqual(factory_cuda.exception.code, "native_cuda_unsuitable")
            self.assertIn(factory_cuda.exception.code, {"runtime_missing", "model_load_failed"})
            self.assertEqual(asr_adapters.FUN_ASR_HF_REV, "d93b302ee7fd505e1b3576120fc142fc6f7820e1")
            hf_dir = tmp / "fun-asr-nano-2512-hf"
            hf_dir.mkdir()
            placeholder = hf_dir / "placeholder"
            placeholder.write_bytes(b"x")
            missing_hf = tmp / "fun-hf-missing.json"
            missing_hf.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "fun-asr-nano-2512",
                        "device": "cuda:0",
                        "language": "en",
                        "runtime": {"python": str(python), "worker": str(worker), "revision": "funasr-python"},
                        "model_path": str(hf_dir),
                        "files": [self._pin(python), self._pin(worker), self._pin(placeholder), self._pin(health)],
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "checkpoint": "Fun-ASR-Nano-2512",
                            "revision": asr_adapters.FUN_ASR_HF_REV,
                        },
                        "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 3 * 1024 * 1024 * 1024},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(asr_manifest.ManifestError) as missing_weights:
                asr_manifest.load_manifest(missing_hf)
            self.assertIn("Fun-ASR-Nano-2512-hf weights", str(missing_weights.exception))
            self.assertIn("encoder.ort", asr_adapters.MOONSHINE_MEDIUM_FILES)
            self.assertNotIn("encoder_model.ort", asr_adapters.MOONSHINE_MEDIUM_FILES)

    def test_moonshine_is_cpu_first_medium_streaming(self):
        self.assertIsNone(asr_adapters.moonshine_identity_error("cpu", {"model_arch": "MEDIUM_STREAMING"}))
        self.assertIn("CPU-first", asr_adapters.moonshine_identity_error("cuda:0", {"model_arch": "MEDIUM_STREAMING"}))
        self.assertIn("MEDIUM_STREAMING", asr_adapters.moonshine_identity_error("cpu", {"model_arch": "SMALL_STREAMING"}))
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            python, worker, model, health = self._base_runtime(tmp)
            ort_dir = tmp / "moonshine-medium"
            ort_dir.mkdir()
            ort_files = []
            for name in asr_adapters.MOONSHINE_MEDIUM_FILES:
                path = ort_dir / name
                path.write_bytes(b"ort")
                ort_files.append(self._pin(path))
            pins = [self._pin(python), self._pin(worker), self._pin(model), self._pin(health)]
            cuda_path = tmp / "moonshine-cuda.json"
            cuda_path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "moonshine-streaming-medium",
                        "device": "cuda:0",
                        "language": "en",
                        "runtime": {"python": str(python), "worker": str(worker), "revision": "test"},
                        "model_path": str(model),
                        "files": pins,
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "model_arch": "MEDIUM_STREAMING",
                        },
                        "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 1},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(asr_manifest.ManifestError):
                asr_manifest.load_manifest(cuda_path)
            for language in ("hi", "auto"):
                path = tmp / f"moonshine-{language}.json"
                path.write_text(
                    json.dumps(
                        {
                            "version": 1,
                            "model_id": "moonshine-streaming-medium",
                            "device": "cpu",
                            "language": language,
                            "runtime": {"python": str(python), "worker": str(worker), "revision": "test"},
                            "model_path": str(model),
                            "files": pins,
                            "options": {
                                "health_audio": str(health),
                                "health_expected": "hello",
                                "model_arch": "MEDIUM_STREAMING",
                            },
                            "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                        }
                    ),
                    encoding="utf-8",
                )
                with self.assertRaises(asr_manifest.ManifestError):
                    asr_manifest.load_manifest(path)
            empty_dir = tmp / "empty-moonshine"
            empty_dir.mkdir()
            missing = asr_manifest.Manifest(
                version=1,
                model_id="moonshine-streaming-medium",
                device="cpu",
                language="en",
                python=python,
                worker=worker,
                revision="test",
                requirements_lock=None,
                model_path=empty_dir,
                artifacts=(),
                options={"model_arch": "MEDIUM_STREAMING"},
                limits={},
                source_path=cuda_path,
            )
            with self.assertRaises(asr_adapters.AdapterError) as missing_files:
                asr_adapters.require_moonshine_medium_identity(missing)
            self.assertEqual(missing_files.exception.code, "artifact_missing")
            missing_pins = tmp / "moonshine-unpinned.json"
            missing_pins.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "moonshine-streaming-medium",
                        "device": "cpu",
                        "language": "en",
                        "runtime": {"python": str(python), "worker": str(worker), "revision": "test"},
                        "model_path": str(ort_dir),
                        "files": pins,
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "model_arch": "MEDIUM_STREAMING",
                        },
                        "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(asr_manifest.ManifestError):
                asr_manifest.load_manifest(missing_pins)
            stub_path = tmp / "moonshine-stub.json"
            stub_doc = {
                "version": 1,
                "model_id": "moonshine-streaming-medium",
                "device": "cpu",
                "language": "en",
                "runtime": {"python": str(python), "worker": str(worker), "revision": asr_adapters.MOONSHINE_VOICE_SOURCE},
                "model_path": str(ort_dir),
                "files": pins + ort_files,
                "options": {
                    "health_audio": str(health),
                    "health_expected": "hello",
                    "model_arch": "MEDIUM_STREAMING",
                },
                "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
            }
            stub_path.write_text(json.dumps(stub_doc), encoding="utf-8")
            with self.assertRaises(asr_manifest.ManifestError) as published:
                asr_manifest.load_manifest(stub_path)
            self.assertIn("encoder.ort", str(published.exception))
            self.assertIn("published artifact", str(published.exception))
            self.assertEqual(len(asr_adapters.moonshine_medium_files(ort_dir)), 7)
            dummy_identity = asr_manifest.Manifest(
                version=1,
                model_id="moonshine-streaming-medium",
                device="cpu",
                language="en",
                python=python,
                worker=worker,
                revision="test",
                requirements_lock=None,
                model_path=ort_dir,
                artifacts=(),
                options={"model_arch": "MEDIUM_STREAMING"},
                limits={},
                source_path=stub_path,
            )
            with self.assertRaises(asr_adapters.AdapterError) as dummy_worker:
                asr_adapters.require_moonshine_medium_identity(dummy_identity)
            self.assertEqual(dummy_worker.exception.code, "wrong_checkpoint")
            self.assertIn("encoder.ort", str(dummy_worker.exception))
            self.assertIn("published artifact", str(dummy_worker.exception))
            with self.assertRaises(asr_manifest.ManifestError) as dummy_pins:
                asr_manifest.verify_moonshine_medium_pins(ort_dir)
            self.assertIn("encoder.ort", str(dummy_pins.exception))
            wrong = dict(stub_doc)
            encoder_sha, encoder_bytes = asr_manifest.MOONSHINE_MEDIUM_PINS["encoder.ort"]
            encoder_path = str(ort_dir / "encoder.ort")
            wrong_files = []
            for item in stub_doc["files"]:
                if item["path"] == encoder_path:
                    wrong_files.append({"path": encoder_path, "sha256": encoder_sha, "bytes": encoder_bytes})
                else:
                    wrong_files.append(item)
            wrong["files"] = wrong_files
            wrong_path = tmp / "moonshine-wrong-hash.json"
            wrong_path.write_text(json.dumps(wrong), encoding="utf-8")
            with self.assertRaises(asr_manifest.ManifestError) as mutated:
                asr_manifest.load_manifest(wrong_path)
            self.assertIn("pinned artifact changed", str(mutated.exception))
            self.assertTrue(asr_adapters.MOONSHINE_VOICE_SOURCE.startswith("https://download.moonshine.ai/"))
            self.assertEqual(asr_manifest.MOONSHINE_MEDIUM_PINS["encoder.ort"][1], 94_202_872)
            self.assertNotIn("encoder_model.ort", asr_manifest.MOONSHINE_MEDIUM_PINS)

    def test_cohere_is_english_only_and_requires_pinned_revision(self):
        self.assertIsNone(asr_adapters.cohere_revision_error({"revision": asr_adapters.COHERE_HF_REV}))
        self.assertIsNotNone(asr_adapters.cohere_revision_error({"revision": "refs/pr/6"}))
        self.assertIsNotNone(asr_adapters.cohere_revision_error({"revision": "0" * 40}))
        gated = asr_adapters._cohere_error(RuntimeError("401 Restricted: gated repo"))
        self.assertEqual(gated.code, "gated_model")
        with tempfile.TemporaryDirectory() as directory:
            tmp = Path(directory)
            python, worker, model, health = self._base_runtime(tmp)
            pins = [self._pin(python), self._pin(worker), self._pin(model), self._pin(health)]
            for language in ("hi", "auto"):
                path = tmp / f"cohere-{language}.json"
                path.write_text(
                    json.dumps(
                        {
                            "version": 1,
                            "model_id": "cohere-transcribe-03-2026",
                            "device": "cpu",
                            "language": language,
                            "runtime": {"python": str(python), "worker": str(worker), "revision": asr_adapters.COHERE_HF_REV},
                            "model_path": str(model),
                            "files": pins,
                            "options": {
                                "health_audio": str(health),
                                "health_expected": "hello",
                                "revision": asr_adapters.COHERE_HF_REV,
                            },
                            "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                        }
                    ),
                    encoding="utf-8",
                )
                with self.assertRaises(asr_manifest.ManifestError):
                    asr_manifest.load_manifest(path)
            floating = tmp / "cohere-pr.json"
            floating.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "cohere-transcribe-03-2026",
                        "device": "cpu",
                        "language": "en",
                        "runtime": {"python": str(python), "worker": str(worker), "revision": "test"},
                        "model_path": str(model),
                        "files": pins,
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "revision": "refs/pr/6",
                        },
                        "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(asr_manifest.ManifestError):
                asr_manifest.load_manifest(floating)
            ok_path = tmp / "cohere-ok.json"
            ok_path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "model_id": "cohere-transcribe-03-2026",
                        "device": "cpu",
                        "language": "en",
                        "runtime": {"python": str(python), "worker": str(worker), "revision": asr_adapters.COHERE_HF_REV},
                        "model_path": str(model),
                        "files": pins,
                        "options": {
                            "health_audio": str(health),
                            "health_expected": "hello",
                            "revision": asr_adapters.COHERE_HF_REV,
                        },
                        "limits": {"memory_bytes": 1024, "tasks": 4, "cpu_cores": 1, "cache_bytes": 1024, "vram_bytes": 0},
                    }
                ),
                encoding="utf-8",
            )
            loaded = asr_manifest.load_manifest(ok_path)
            self.assertEqual(loaded.options["revision"], asr_adapters.COHERE_HF_REV)
            self.assertEqual(loaded.language, "en")
            asr_adapters.require_cohere_english_identity(loaded)
            other_rev = asr_manifest.Manifest(
                version=1,
                model_id="cohere-transcribe-03-2026",
                device="cpu",
                language="en",
                python=python,
                worker=worker,
                revision=asr_adapters.COHERE_HF_REV,
                requirements_lock=None,
                model_path=model,
                artifacts=(),
                options={"revision": "0" * 40},
                limits={},
                source_path=ok_path,
            )
            with self.assertRaises(asr_adapters.AdapterError) as worker_rev:
                asr_adapters.require_cohere_english_identity(other_rev)
            self.assertEqual(worker_rev.exception.code, "invalid_setting")
            self.assertIn("b1eacc2686a3d08ceaae5f24a88b1d519620bc09", str(worker_rev.exception))


if __name__ == "__main__":
    unittest.main()
