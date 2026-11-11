import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock
import wave


ASR_ROOT = Path(__file__).parents[1]
if str(ASR_ROOT) not in sys.path:
    sys.path.insert(0, str(ASR_ROOT))


def _load(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, ASR_ROOT / filename)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


benchmark = _load("benchmark", "benchmark.py")
cloud_bench = _load("cloud_bench", "cloud_bench.py")

REPO_ROOT = ASR_ROOT.parents[1]
KEY = "sk-test-secret-0123456789"


def make_wav(path: Path, frames: int = 1600) -> None:
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(16_000)
        handle.writeframes(b"\0\0" * frames)


def make_corpus(tmp: Path) -> dict:
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
    (tmp / "refs.json").write_text(json.dumps(refs), encoding="utf-8")
    (tmp / "consent.json").write_text(json.dumps({"consent_id": "c-1"}), encoding="utf-8")
    inventory = benchmark.inventory(audio_dir, tmp / "refs.json", tmp / "consent.json", "cloud-bench-test")
    frozen = benchmark.freeze(inventory, "test-seed", 0.5)
    corpus_path = tmp / "corpus.json"
    benchmark.write_json(corpus_path, frozen)
    return frozen


class FakeProvider:
    """Local HTTP server. `script[path]` is a list of (status, headers, body)
    answers popped in order; the last one repeats."""

    def __init__(self):
        self.script: dict[str, list] = {}
        self.requests: list[dict] = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def _serve(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                path = self.path.split("?")[0]
                outer.requests.append({"method": self.command, "path": path, "query": self.path.partition("?")[2], "headers": dict(self.headers), "body": body})
                answers = outer.script.get(path, [(200, {}, {})])
                status, headers, payload = answers.pop(0) if len(answers) > 1 else answers[0]
                data = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                for name, value in headers.items():
                    self.send_header(name, value)
                self.end_headers()
                self.wfile.write(data)

            do_POST = _serve
            do_GET = _serve

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=0.02), daemon=True)

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.server.shutdown()
        self.server.server_close()


def run_cli(*args: str) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = cloud_bench.main(list(args))
    return code, out.getvalue(), err.getvalue()


class ScoreTests(unittest.TestCase):
    def test_score_known_wer_negative_hallucination_and_critical_miss(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            corpus = make_corpus(tmp)
            by_lang = {r["language"]: r for r in corpus["records"]}

            def row(language, text, latency):
                record = by_lang[language]
                return {"record_id": record["record_id"], "candidate": "fake", "repeat": 0, "status": "ok", "text": text, "latency_ms": latency, "audio_duration_ms": record["duration_ms"]}

            run = {
                "schema_version": 1,
                "kind": "cloud_bench_run",
                "corpus_manifest_sha256": corpus["manifest_sha256"],
                "rows": [row("english", "Ship the", 100), row("hinglish", "kal deploy karna", 300), row("negative", "Thank you", 200)],
            }
            run["rows"].append({"record_id": by_lang["english"]["record_id"], "candidate": "fake", "repeat": 1, "status": "error", "text": None, "error": "HTTP 500", "latency_ms": 5, "audio_duration_ms": by_lang["english"]["duration_ms"]})
            run_path, report_path = tmp / "run.json", tmp / "report.json"
            benchmark.write_json(run_path, run)
            code, out, _ = run_cli("score", "--corpus", str(tmp / "corpus.json"), "--run", str(run_path), "--output", str(report_path))
            self.assertEqual(code, 0)
            report = benchmark.read_json(report_path)
            cand = report["candidates"]["fake"]
            english = cand["by_language"]["english"]
            self.assertEqual((english["deletions"], english["substitutions"], english["insertions"]), (1, 0, 0))
            self.assertAlmostEqual(english["wer"], 1 / 3)
            self.assertEqual(english["errors"], 1)
            self.assertEqual(cand["by_language"]["hinglish"]["wer"], 0.0)
            negative = cand["negative"]
            self.assertEqual((negative["rows_with_output"], negative["inserted_tokens"]), (1, 2))
            self.assertEqual(cand["critical_tokens"], {"records": 1, "all_present": 0, "fraction": 0.0})
            speech = cand["speech"]
            self.assertAlmostEqual(speech["wer"], 1 / 6)
            self.assertEqual(speech["latency_ms"], {"p50": 100, "p90": 300, "max": 300})
            self.assertEqual(sorted(cand["by_split"]), ["development", "heldout"])
            # Aggregates and numeric per-record results only: no text.
            blob = json.dumps(report).lower()
            for leaked in ("ship the", "release", "kal deploy", "thank you"):
                self.assertNotIn(leaked, blob)
            self.assertIn("fake", out)
            self.assertIn("negative", out)

    def test_score_rejects_run_for_another_corpus(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            make_corpus(tmp)
            benchmark.write_json(tmp / "run.json", {"schema_version": 1, "kind": "cloud_bench_run", "corpus_manifest_sha256": "0" * 64, "rows": []})
            code, _, err = run_cli("score", "--corpus", str(tmp / "corpus.json"), "--run", str(tmp / "run.json"), "--output", str(tmp / "r.json"))
            self.assertEqual(code, 2)
            self.assertIn("different corpus", err)


class TranscribeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.corpus = make_corpus(self.dir)
        self.corpus_path = str(self.dir / "corpus.json")
        self.run_path = self.dir / "run.json"
        env = {
            "GROQ_API_KEY": KEY,
            "DEEPGRAM_API_KEY": KEY,
            "ELEVENLABS_API_KEY": KEY,
            "ASSEMBLYAI_API_KEY": KEY,
            "CLOUDFLARE_API_TOKEN": KEY,
            "CLOUDFLARE_ACCOUNT_ID": "acct",
            "CLOUD_BENCH_POLL_SECONDS": "0",
        }
        patcher = mock.patch.dict(os.environ, env)
        patcher.start()
        self.addCleanup(patcher.stop)

    def transcribe(self, provider: FakeProvider, candidate: str, url_env: str, *extra: str):
        with mock.patch.dict(os.environ, {url_env: provider.url}):
            return run_cli("transcribe", "--corpus", self.corpus_path, "--candidate", candidate, "--output", str(self.run_path), "--upload-format", "wav", *extra)

    def rows(self):
        return benchmark.read_json(self.run_path)["rows"]

    def test_groq_request_shape_and_row(self):
        with FakeProvider() as fake:
            fake.script["/audio/transcriptions"] = [(200, {}, {"text": "hello there"})]
            code, _, err = self.transcribe(fake, "groq-whisper-large-v3", "CLOUD_BENCH_GROQ_URL", "--language", "english")
            self.assertEqual(code, 0, err)
            request = fake.requests[0]
            self.assertEqual(request["headers"]["Authorization"], f"Bearer {KEY}")
            body = request["body"]
            for needle in (b'name="model"', b"whisper-large-v3", b"verbose_json", b'name="temperature"', b'name="language"', b"filename=\"recording.wav\""):
                self.assertIn(needle, body)
        row = self.rows()[0]
        self.assertEqual((row["text"], row["status"], row["http_status"], row["upload_format"], row["repeat"]), ("hello there", "ok", 200, "wav", 0))
        self.assertGreaterEqual(row["latency_ms"], 0)
        self.assertEqual(row["audio_duration_ms"], 100)
        self.assertNotIn(KEY, self.run_path.read_text())

    def test_deepgram_prerecorded_shape(self):
        with FakeProvider() as fake:
            fake.script["/v1/listen"] = [(200, {}, {"results": {"channels": [{"alternatives": [{"transcript": "kal deploy"}]}]}})]
            code, _, err = self.transcribe(fake, "deepgram-nova-3", "CLOUD_BENCH_DEEPGRAM_URL", "--language", "hinglish", "--limit", "1")
            self.assertEqual(code, 0, err)
            request = fake.requests[0]
            self.assertEqual(request["headers"]["Authorization"], f"Token {KEY}")
            self.assertEqual(request["headers"]["Content-Type"], "audio/wav")
            for param in ("model=nova-3", "smart_format=true", "punctuate=true", "language=en"):
                self.assertIn(param, request["query"])
        row = self.rows()[0]
        self.assertEqual((row["text"], row["mode"]), ("kal deploy", "prerecorded"))

    def test_429_honours_retry_after_then_succeeds(self):
        with FakeProvider() as fake:
            fake.script["/audio/transcriptions"] = [(429, {"Retry-After": "0"}, {"error": {"message": "slow down"}}), (200, {}, {"text": "ok now"})]
            waits = []
            with mock.patch.object(cloud_bench, "_sleep", waits.append):
                code, _, err = self.transcribe(fake, "groq-whisper-large-v3", "CLOUD_BENCH_GROQ_URL", "--language", "hinglish")
            self.assertEqual(code, 0, err)
            self.assertEqual(len(fake.requests), 2)
        row = self.rows()[0]
        self.assertEqual((row["status"], row["text"], row["retries"]), ("ok", "ok now", 1))
        self.assertEqual(waits, [0.0])

    def test_hard_error_is_recorded_without_secrets(self):
        with FakeProvider() as fake:
            fake.script["/audio/transcriptions"] = [(500, {}, {"error": f"boom {KEY}"})]
            code, _, _ = self.transcribe(fake, "groq-whisper-large-v3", "CLOUD_BENCH_GROQ_URL", "--language", "hinglish")
            self.assertEqual(code, 1)
        row = self.rows()[0]
        self.assertEqual((row["status"], row["text"], row["http_status"]), ("error", None, 500))
        self.assertIn("500", row["error"])
        self.assertNotIn(KEY, self.run_path.read_text())

    def test_resume_skips_ok_rows_and_retries_errors(self):
        with FakeProvider() as fake:
            fake.script["/audio/transcriptions"] = [(500, {}, {"error": "boom"})]
            self.transcribe(fake, "groq-whisper-large-v3", "CLOUD_BENCH_GROQ_URL", "--language", "english")
            self.assertEqual([r["status"] for r in self.rows()], ["error"])
            fake.script["/audio/transcriptions"] = [(200, {}, {"text": "fine"})]
            self.transcribe(fake, "groq-whisper-large-v3", "CLOUD_BENCH_GROQ_URL", "--language", "english")
            self.assertEqual([r["status"] for r in self.rows()], ["ok"])
            before = len(fake.requests)
            code, _, _ = self.transcribe(fake, "groq-whisper-large-v3", "CLOUD_BENCH_GROQ_URL", "--language", "english")
            self.assertEqual(code, 0)
            self.assertEqual(len(fake.requests), before)
            self.assertEqual(len(self.rows()), 1)

    def test_all_records_with_concurrency_and_repeats(self):
        with FakeProvider() as fake:
            fake.script["/audio/transcriptions"] = [(200, {}, {"text": "x"})]
            code, _, err = self.transcribe(fake, "groq-whisper-large-v3-turbo", "CLOUD_BENCH_GROQ_URL", "--concurrency", "3", "--repeats", "2")
            self.assertEqual(code, 0, err)
        rows = self.rows()
        self.assertEqual(len(rows), 6)
        self.assertEqual({(r["record_id"], r["repeat"]) for r in rows}.__len__(), 6)
        self.assertTrue(all(b'whisper-large-v3-turbo' in r["body"] for r in fake.requests))

    def test_missing_key_fails_up_front(self):
        with FakeProvider() as fake:
            with mock.patch.dict(os.environ, {}, clear=False):
                os.environ.pop("GROQ_API_KEY")
                code, _, err = self.transcribe(fake, "groq-whisper-large-v3", "CLOUD_BENCH_GROQ_URL")
            self.assertEqual(code, 2)
            self.assertIn("GROQ_API_KEY", err)
            self.assertEqual(fake.requests, [])
        self.assertFalse(self.run_path.exists())

    def test_refuses_run_path_in_unignored_git_tree(self):
        target = REPO_ROOT / "tools" / "local-asr" / "cloud_bench_refused_run.json"
        self.assertFalse(target.exists())
        with FakeProvider() as fake:
            with mock.patch.dict(os.environ, {"CLOUD_BENCH_GROQ_URL": fake.url}):
                code, _, err = run_cli("transcribe", "--corpus", self.corpus_path, "--candidate", "groq-whisper-large-v3", "--output", str(target))
            self.assertEqual(code, 2)
            self.assertIn("git", err)
            self.assertEqual(fake.requests, [])
        self.assertFalse(target.exists())

    def test_elevenlabs_shape(self):
        with FakeProvider() as fake:
            fake.script["/v1/speech-to-text"] = [(200, {}, {"text": "hi"})]
            code, _, err = self.transcribe(fake, "elevenlabs-scribe-v2", "CLOUD_BENCH_ELEVENLABS_URL", "--limit", "1")
            self.assertEqual(code, 0, err)
            request = fake.requests[0]
            self.assertEqual({k.lower(): v for k, v in request["headers"].items()}["xi-api-key"], KEY)
            for needle in (b'name="model_id"', b"scribe_v2", b'name="language_code"', b'name="tag_audio_events"'):
                self.assertIn(needle, request["body"])
        self.assertEqual(self.rows()[0]["text"], "hi")

    def test_cloudflare_shape(self):
        with FakeProvider() as fake:
            path = "/accounts/acct/ai/run/@cf/openai/whisper-large-v3-turbo"
            fake.script[path] = [(200, {}, {"result": {"text": "cf text"}, "success": True})]
            code, _, err = self.transcribe(fake, "cloudflare-whisper-large-v3-turbo", "CLOUD_BENCH_CLOUDFLARE_URL", "--limit", "1")
            self.assertEqual(code, 0, err)
            request = fake.requests[0]
            self.assertEqual(request["headers"]["Authorization"], f"Bearer {KEY}")
            body = json.loads(request["body"])
            self.assertIsInstance(body["audio"], str)
            self.assertEqual(body["language"], "en")
        self.assertEqual(self.rows()[0]["text"], "cf text")

    def test_assemblyai_upload_create_poll(self):
        with FakeProvider() as fake:
            fake.script["/v2/upload"] = [(200, {}, {"upload_url": "https://cdn.example/audio"})]
            fake.script["/v2/transcript"] = [(200, {}, {"id": "t1", "status": "queued"})]
            fake.script["/v2/transcript/t1"] = [(200, {}, {"status": "processing"}), (200, {}, {"status": "completed", "text": "assembled"})]
            code, _, err = self.transcribe(fake, "assemblyai-universal", "CLOUD_BENCH_ASSEMBLYAI_URL", "--limit", "1")
            self.assertEqual(code, 0, err)
            self.assertEqual([r["path"] for r in fake.requests], ["/v2/upload", "/v2/transcript", "/v2/transcript/t1", "/v2/transcript/t1"])
            self.assertEqual(fake.requests[0]["headers"]["Authorization"], KEY)
            create = json.loads(fake.requests[1]["body"])
            self.assertEqual(create["audio_url"], "https://cdn.example/audio")
            self.assertEqual(create["speech_models"], ["universal-2"])
        self.assertEqual(self.rows()[0]["text"], "assembled")


if __name__ == "__main__":
    unittest.main()
