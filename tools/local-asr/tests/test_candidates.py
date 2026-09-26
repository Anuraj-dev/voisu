import dataclasses
import hashlib
import importlib.util
import json
import re
import subprocess
import sys
import tempfile
import unittest
import wave
from pathlib import Path
from unittest import mock

ASR_ROOT = Path(__file__).parents[1]
if str(ASR_ROOT) not in sys.path:
    sys.path.insert(0, str(ASR_ROOT))
from voisu_asr import adapters as asr_adapters
from voisu_asr import manifest as asr_manifest
from voisu_asr.candidates import CANDIDATES, MIB

SPEC = importlib.util.spec_from_file_location("voisu_local_asr_benchmark_candidates", ASR_ROOT / "benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(benchmark)

RUST_CATALOG = ASR_ROOT.parents[1] / "crates/voisu-app/src/local_model/development.rs"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write_wav(path: Path) -> None:
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(16_000)
        handle.writeframes(b"\0\0" * 1600)


class RealCandidate:
    """Real files with real digests for every name a published candidate pins.

    The candidate table is patched to those digests, so load_manifest hashes
    the bytes exactly as it would the published artifacts.
    """

    def __init__(self, root: Path, model_id: str, device: str = "cuda:0"):
        published = CANDIDATES[model_id]
        model_dir = root / "model"
        model_dir.mkdir()
        files = []
        for name, _sha, _size in published.files:
            data = f"{model_id}/{name}".encode()
            (model_dir / name).write_bytes(data)
            files.append((name, sha256(data), len(data)))
        self.candidate = dataclasses.replace(published, files=tuple(files))
        worker = root / "worker.py"
        worker.write_text("# worker\n")
        health = root / "health.wav"
        write_wav(health)
        pins = [Path(sys.executable), worker, health, *(model_dir / name for name, _, _ in files)]
        model_path = model_dir / published.weights if published.backend == "whisper.cpp" else model_dir
        self.document = {
            "version": 1,
            "model_id": model_id,
            "device": device,
            "language": sorted(published.language_codes)[0],
            "runtime": {"python": sys.executable, "worker": str(worker), "revision": "test"},
            "model_path": str(model_path),
            "files": [{"path": str(p), "sha256": sha256(p.read_bytes()), "bytes": p.stat().st_size} for p in pins],
            "options": {
                "revision": published.revision,
                "health_audio": str(health),
                "health_expected": "hello",
            },
            "limits": {
                "memory_bytes": 1,
                "tasks": 1,
                "cpu_cores": 1,
                "cache_bytes": 1,
                "vram_bytes": published.min_vram_bytes if device != "cpu" else 0,
            },
        }
        self.path = root / "manifest.json"
        self.model_dir = model_dir

    def load(self, **changes):
        document = json.loads(json.dumps(self.document))
        for key, value in changes.items():
            section, _, field = key.partition(".")
            if field:
                document[section][field] = value
            else:
                document[section] = value
        self.path.write_text(json.dumps(document))
        with mock.patch.dict(CANDIDATES, {self.candidate.model_id: self.candidate}):
            return asr_manifest.load_manifest(self.path)


class CandidateRegistryTest(unittest.TestCase):
    def test_every_candidate_is_registered_everywhere(self):
        catalog = RUST_CATALOG.read_text(encoding="utf-8")
        for model_id in CANDIDATES:
            self.assertIn(model_id, asr_manifest.MODEL_IDS)
            self.assertIn(model_id, benchmark.EXPECTED_MODELS)
            self.assertIn(model_id, asr_adapters.ADAPTER_LOADERS)
            self.assertIn(f'"{model_id}"', catalog)

    def test_rust_and_python_pins_agree(self):
        source = RUST_CATALOG.read_text(encoding="utf-8")
        table = source.split("pub static CANDIDATE_PINS", 1)[1].split("\n];", 1)[0]
        rust = {}
        for block in table.split("CandidatePin {")[1:]:
            field = lambda name: re.search(rf'{name}: "([^"]+)"', block).group(1)  # noqa: E731
            files = tuple(
                (name, sha, int(size.replace("_", "")))
                for name, sha, size in re.findall(r'\(\s*"([^"]+)",\s*"([0-9a-f]{64})",\s*([0-9_]+),?\s*\)', block)
            )
            vram = int(re.search(r"min_vram_bytes: (\d+) \* 1024 \* 1024", block).group(1)) * MIB
            precision = re.search(r'precision: Some\(\(\s*"([^"]+)",\s*"([^"]+)",\s*"([^"]+)",?\s*\)\)', block)
            rust[field("model_id")] = (
                field("revision"),
                field("weights"),
                files,
                vram,
                precision.groups() if precision else None,
            )
        python = {
            item.model_id: (item.revision, item.weights, item.files, item.min_vram_bytes, item.precision)
            for item in CANDIDATES.values()
        }
        self.assertEqual(rust, python)

    def test_published_pins_are_well_formed(self):
        for item in CANDIDATES.values():
            self.assertRegex(item.revision, r"^[0-9a-f]{40}$")
            names = [name for name, _, _ in item.files]
            self.assertIn(item.weights, names)
            self.assertEqual(len(names), len(set(names)))
            for name, sha, size in item.files:
                self.assertRegex(sha, r"^[0-9a-f]{64}$", name)
                self.assertGreater(size, 0)
            self.assertGreater(item.min_vram_bytes, 0)
        for model_id in ("faster-whisper-large-v3-turbo-int8", "whisper-hindi2hinglish-apex-int8", "distil-large-v3.5-int8"):
            self.assertEqual(
                {name for name, _, _ in CANDIDATES[model_id].files},
                {"config.json", "model.bin", "preprocessor_config.json", "tokenizer.json", "vocabulary.json"},
            )
            self.assertEqual(CANDIDATES[model_id].precision, ("compute_type", "int8_float16", "int8"))

    def test_hinglish_model_writes_latin_script_under_the_english_token(self):
        apex = CANDIDATES["whisper-hindi2hinglish-apex-int8"]
        self.assertEqual(apex.whisper_language("hi"), "en")
        self.assertEqual(apex.whisper_language("en"), "en")
        self.assertNotIn("auto", apex.language_codes)
        self.assertIsNone(CANDIDATES["whisper-cpp-large-v3-turbo-q5-0"].whisper_language("auto"))
        for english_only in ("distil-large-v3.5-int8", "granite-speech-5.0-470m-turboctc"):
            self.assertEqual(CANDIDATES[english_only].language_codes, frozenset({"en"}))


class CandidateManifestTest(unittest.TestCase):
    def test_real_bytes_load_and_changed_bytes_are_rejected(self):
        for model_id in CANDIDATES:
            with self.subTest(model_id), tempfile.TemporaryDirectory() as temp:
                real = RealCandidate(Path(temp), model_id)
                manifest = real.load()
                self.assertEqual(manifest.model_id, model_id)
                (real.model_dir / CANDIDATES[model_id].weights).write_bytes(b"other weights")
                with self.assertRaises(asr_manifest.ManifestError):
                    real.load()

    def test_every_loader_file_must_be_pinned_at_its_digest(self):
        with tempfile.TemporaryDirectory() as temp:
            real = RealCandidate(Path(temp), "distil-large-v3.5-int8")
            files = real.document["files"]
            for index in range(3, len(files)):
                with self.assertRaisesRegex(asr_manifest.ManifestError, "must be pinned"):
                    real.load(files=files[:index] + files[index + 1 :])

    def test_unmeasured_revision_precision_vram_and_language_are_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            real = RealCandidate(Path(temp), "distil-large-v3.5-int8")
            options = real.document["options"]
            real.load(options={**options, "compute_type": "int8_float16"})
            for bad, pattern in (
                ({**options, "revision": "main"}, "revision"),
                ({**options, "compute_type": "int8"}, "measured only"),
                ({**options, "compute_type": "float16"}, "measured only"),
            ):
                with self.assertRaisesRegex(asr_manifest.ManifestError, pattern):
                    real.load(options=bad)
            below = CANDIDATES["distil-large-v3.5-int8"].min_vram_bytes - 1
            with self.assertRaisesRegex(asr_manifest.ManifestError, "below its measured peak"):
                real.load(**{"limits.vram_bytes": below})
            with self.assertRaisesRegex(asr_manifest.ManifestError, "language"):
                real.load(language="hi")
        with tempfile.TemporaryDirectory() as temp:
            real = RealCandidate(Path(temp), "granite-speech-5.0-470m-turboctc", device="cpu")
            options = real.document["options"]
            real.load(options={**options, "dtype": "float32"})
            with self.assertRaisesRegex(asr_manifest.ManifestError, "measured only"):
                real.load(options={**options, "dtype": "bfloat16"})

    def test_decoding_settings_are_not_manifest_options(self):
        with tempfile.TemporaryDirectory() as temp:
            real = RealCandidate(Path(temp), "whisper-cpp-small-q8-0")
            with self.assertRaisesRegex(asr_manifest.ManifestError, "unknown"):
                real.load(options={**real.document["options"], "beam_size": 1})


class CandidateAdapterTest(unittest.TestCase):
    def test_missing_runtime_is_an_explicit_error_not_a_fallback(self):
        for model_id, runtime in (("whisper-cpp-small-q8-0", "pywhispercpp"), ("distil-large-v3.5-int8", "faster_whisper")):
            if importlib.util.find_spec(runtime) is not None:
                continue
            with tempfile.TemporaryDirectory() as temp:
                real = RealCandidate(Path(temp), model_id, device="cpu")
                manifest = real.load()
                with mock.patch.dict(CANDIDATES, {model_id: real.candidate}):
                    with self.assertRaises(asr_adapters.AdapterError) as caught:
                        asr_adapters.create_adapter(manifest)
                self.assertEqual(caught.exception.code, "runtime_missing")

    def test_adapter_rechecks_identity_of_an_unvalidated_manifest(self):
        with tempfile.TemporaryDirectory() as temp:
            real = RealCandidate(Path(temp), "whisper-cpp-small-q8-0", device="cpu")
            manifest = real.load()
            with mock.patch.dict(CANDIDATES, {manifest.model_id: real.candidate}):
                changed = dataclasses.replace(manifest, options={**manifest.options, "revision": "main"})
                with self.assertRaises(asr_adapters.AdapterError) as caught:
                    asr_adapters.create_adapter(changed)
                self.assertEqual(caught.exception.code, "wrong_checkpoint")
                unpinned = dataclasses.replace(manifest, artifacts=())
                with self.assertRaises(asr_adapters.AdapterError) as caught:
                    asr_adapters.create_adapter(unpinned)
                self.assertEqual(caught.exception.code, "wrong_checkpoint")


class WorkerStdoutTest(unittest.TestCase):
    def test_buffered_native_printf_never_reaches_the_protocol_stream(self):
        # C stdio fully buffers a pipe, so this printf is flushed only at
        # process exit: after every scoped redirect would have been undone.
        script = (
            "import ctypes, importlib.util, sys\n"
            "spec = importlib.util.spec_from_file_location('voisu_worker', sys.argv[1])\n"
            "worker = importlib.util.module_from_spec(spec); spec.loader.exec_module(worker)\n"
            "protocol = worker.claim_protocol_stdout()\n"
            "libc = ctypes.CDLL(None)\n"
            "libc.printf(b'native-before-frame ')\n"
            "print('python-print')\n"
            "protocol.write(b'FRAME'); protocol.flush()\n"
            "libc.printf(b'native-after-frame\\n')\n"
        )
        result = subprocess.run(
            [sys.executable, "-c", script, str(ASR_ROOT / "worker.py")],
            capture_output=True,
            check=True,
        )
        self.assertEqual(result.stdout, b"FRAME")
        self.assertIn(b"native-before-frame", result.stderr)
        self.assertIn(b"native-after-frame", result.stderr)
        self.assertIn(b"python-print", result.stderr)


if __name__ == "__main__":
    unittest.main()
