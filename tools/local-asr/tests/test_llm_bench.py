import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock


MODULE_PATH = Path(__file__).parents[1] / "llm_bench.py"
SPEC = importlib.util.spec_from_file_location("voisu_llm_bench", MODULE_PATH)
llm_bench = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(llm_bench)

SECRET = "sk-test-SECRET-1234"

# A stand-in for the Rust helper that speaks the same JSON-line protocol.
STUB_RUST = textwrap.dedent(
    '''
    import json, sys
    for line in sys.stdin:
        msg = json.loads(line)
        a, b = msg.get("deepgram", msg.get("text", "")), msg.get("groq")
        if msg["op"] == "request":
            if a == b:
                out = {"ok": True, "eligible": False, "body": None, "text": a}
            else:
                body = {"model": "production-default", "reasoning_effort": "none", "temperature": 0,
                        "sentinel": "from-rust", "messages": [{"role": "user", "content": json.dumps({"deepgram": a, "groq": b or ""})}]}
                out = {"ok": True, "eligible": True, "eligibility": "material_disagreement", "body": body}
        else:
            raw = msg["response_body"]
            try:
                wording = json.loads(json.loads(raw)["choices"][0]["message"]["content"])["wording"]
                out = {"ok": True, "text": wording, "outcome": "accepted"}
            except Exception:
                out = {"ok": True, "text": a, "outcome": "failed"}
        out["id"] = msg["id"]
        print(json.dumps(out), flush=True)
    '''
)


def completion(wording, usage=True):
    body = {"choices": [{"message": {"role": "assistant", "content": json.dumps({"wording": wording})}}]}
    if usage:
        body["usage"] = {"prompt_tokens": 50, "completion_tokens": 12, "completion_tokens_details": {"reasoning_tokens": 4}}
    return body


class FakeProvider:
    """Local chat-completions endpoint. script maps a deepgram text to queued actions."""

    def __init__(self):
        self.script = {}
        self.requests = []
        self.release = threading.Event()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length))
                outer.requests.append({"auth": self.headers.get("Authorization"), "body": body, "agent": self.headers.get("User-Agent")})
                key = json.loads(body["messages"][0]["content"])["deepgram"]
                action = outer.script.get(key, [("ok", "default wording")]).pop(0) if outer.script.get(key) else ("ok", "default wording")
                kind = action[0]
                if kind == "hang":
                    outer.release.wait(10)
                    return
                if kind == "429":
                    self.send_response(429)
                    self.send_header("Retry-After", action[1])
                    payload = b'{"error":"rate limited"}'
                elif kind == "500":
                    self.send_response(500)
                    payload = json.dumps({"error": f"upstream failure for {outer_key()}"}).encode()
                else:
                    self.send_response(200)
                    payload = json.dumps(completion(action[1])).encode()
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        def outer_key():
            return SECRET  # a hostile server that echoes the key back

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/openai/v1/chat/completions"

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()


def write(path, value):
    path.write_text(json.dumps(value), encoding="utf-8")
    return path


def stt_row(record_id, candidate, text, status="ok", repeat=0):
    return {"record_id": record_id, "candidate": candidate, "repeat": repeat, "status": status, "text": text, "latency_ms": 10}


class ScoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        tmp = Path(self.tmp.name)
        self.corpus = write(tmp / "corpus.json", {"records": [
            {"record_id": "r1", "language": "english", "reference": "ship the release now", "split": "development", "critical_tokens": ["release"]},
            {"record_id": "r2", "language": "english", "reference": "open the config file", "split": "development"},
            {"record_id": "r3", "language": "english", "reference": "deploy to staging today", "split": "development", "critical_tokens": ["today"]},
            {"record_id": "h1", "language": "hinglish", "reference": "kal deploy karna", "split": "heldout"},
        ]})
        a = {"r1": "ship the release now", "r2": "open the con fig file", "r3": "deploy to staging today", "h1": "kal deploy karna"}
        b = {"r1": "ship a release now", "r2": "open the confit file", "r3": "deploy to stage in today", "h1": "kal deploy karna"}
        rows = [stt_row(r, "dg", t) for r, t in a.items()] + [stt_row(r, "gq", t) for r, t in b.items()]
        rows.append(stt_row("r1", "dg", "ignored repeat", repeat=1))
        self.stt = write(tmp / "stt.json", {"rows": rows})
        outputs = {"r1": "ship the release now", "r2": "open the config file", "r3": "deploy to staging", "h1": "kal deploy zebra karna"}
        latencies = {"r1": 100, "r2": 200, "r3": 300, "h1": 400}
        llm_rows = [
            {"record_id": r, "model": "groq:m", "repeat": 0, "status": "ok", "text": t, "raw_status": "accepted",
             "latency_ms": latencies[r], "completion_tokens": 10 + i, "reasoning_tokens": 2, "retries": 0}
            for i, (r, t) in enumerate(outputs.items())
        ]
        llm_rows.append({"record_id": "r1", "model": "groq:m", "repeat": 1, "status": "error", "text": None, "raw_status": "timeout", "latency_ms": None, "retries": 1})
        self.llm = write(tmp / "llm.json", {"task": "intent-reconstruction", "source_a": "dg", "source_b": "gq", "rows": llm_rows})
        self.report_path = tmp / "report.json"

    def score(self, *extra):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = llm_bench.main(["score", "--corpus", str(self.corpus), "--llm-run", str(self.llm),
                                   "--stt-run", str(self.stt), "--output", str(self.report_path), *extra])
        self.assertEqual(code, 0)
        return json.loads(self.report_path.read_text()), out.getvalue()

    def test_pooled_wer_baselines_and_verdicts(self):
        report, _ = self.score()
        model = report["models"]["groq:m"]
        english = model["strata"]["english"]
        self.assertEqual(english["samples"], 3)
        self.assertAlmostEqual(english["wer"]["model"], 1 / 12)
        self.assertAlmostEqual(english["wer"]["dg"], 2 / 12)
        self.assertAlmostEqual(english["wer"]["gq"], 4 / 12)
        self.assertEqual(english["verdicts"], {"better": 1, "worse": 1, "unchanged": 1})
        hinglish = model["strata"]["hinglish"]
        self.assertAlmostEqual(hinglish["wer"]["model"], 1 / 3)
        self.assertEqual(hinglish["verdicts"], {"better": 0, "worse": 1, "unchanged": 0})
        self.assertEqual(model["strata"]["all"]["samples"], 4)

    def test_word_fidelity_counters_and_critical_tokens(self):
        report, _ = self.score()
        english = report["models"]["groq:m"]["strata"]["english"]
        # r2: "config" is correct but appears in neither source ("con fig" / "confit"), so it counts as invented.
        self.assertEqual(english["invented_tokens"], 1)
        self.assertEqual(english["dropped_agreed_tokens"], 1)  # "today" is in the reference and both sources
        self.assertAlmostEqual(english["critical_token_hit_rate"]["model"], 0.5)
        hinglish = report["models"]["groq:m"]["strata"]["hinglish"]
        self.assertEqual(hinglish["invented_tokens"], 1)  # "zebra" is in neither source
        self.assertEqual(hinglish["dropped_agreed_tokens"], 0)

    def test_errors_latency_and_tokens(self):
        report, _ = self.score("--deadline-ms", "250")
        model = report["models"]["groq:m"]
        self.assertEqual((model["errors"], model["timeouts"], model["retries_429"]), (1, 1, 1))
        self.assertEqual(model["latency_ms"]["p50"], 200)
        self.assertEqual(model["latency_ms"]["p90"], 400)
        self.assertEqual(model["latency_ms"]["max"], 400)
        self.assertEqual(model["latency_ms"]["over_deadline"], 2)
        self.assertAlmostEqual(model["mean_completion_tokens"], 11.5)

    def test_report_has_no_transcript_text_and_table_is_printed(self):
        report, table = self.score()
        raw = self.report_path.read_text()
        for secret_text in ("ship the release", "kal deploy", "zebra", "staging"):
            self.assertNotIn(secret_text, raw)
        self.assertEqual(len(report["per_record"]), 4)
        self.assertIn("groq:m", table)
        self.assertIn("hinglish", table)


class RunTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.provider = FakeProvider()
        self.addCleanup(self.provider.close)
        (self.dir / "stub.py").write_text(STUB_RUST, encoding="utf-8")
        self.rust_cmd = f"{sys.executable} {self.dir / 'stub.py'}"
        self.corpus = write(self.dir / "corpus.json", {"records": [
            {"record_id": "a", "language": "english", "reference": "x", "split": "development"},
            {"record_id": "b", "language": "english", "reference": "x", "split": "development"},
            {"record_id": "same", "language": "english", "reference": "x", "split": "development"},
            {"record_id": "gone", "language": "english", "reference": "x", "split": "development"},
            {"record_id": "neg", "language": "negative", "reference": "", "split": "development"},
        ]})
        rows = []
        for rid, a, b in (("a", "alpha one", "alpha won"), ("b", "bravo one", "bravo won"), ("same", "same text", "same text"), ("neg", "q", "r")):
            rows += [stt_row(rid, "dg", a), stt_row(rid, "gq", b)]
        rows += [stt_row("gone", "dg", "only one source")]
        self.stt = write(self.dir / "stt.json", {"rows": rows})
        self.output = self.dir / "llm_run.json"
        self.sleeps = []
        patches = [
            mock.patch.dict(os.environ, {"GROQ_API_KEY": SECRET, "LLM_BENCH_GROQ_URL": self.provider.url}, clear=False),
            mock.patch.object(llm_bench, "SLEEP", self.sleeps.append),
        ]
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)

    def run_cli(self, *extra, models=("groq:qwen/qwen3.8-27b",), output=None):
        argv = ["run", "--corpus", str(self.corpus), "--stt-run", str(self.stt), "--task", "intent-reconstruction",
                "--source-a", "dg", "--source-b", "gq", "--output", str(output or self.output),
                "--rust-cmd", self.rust_cmd, "--timeout-seconds", "5", "--split", "development"]
        for model in models:
            argv += ["--model", model]
        argv += list(extra)
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = llm_bench.main(argv)
        self.last_output = out.getvalue() + err.getvalue()
        return code

    def rows(self):
        return json.loads(self.output.read_text())["rows"]

    def test_ok_run_uses_rust_body_and_model_overrides(self):
        self.provider.script = {"alpha one": [("ok", "alpha one fixed")], "bravo one": [("ok", "bravo one fixed")]}
        code = self.run_cli(models=("groq:qwen/qwen3.8-27b", "groq:openai/gpt-oss-120b"))
        self.assertEqual(code, 0, self.last_output)
        document = json.loads(self.output.read_text())
        self.assertEqual(document["skipped"], {"missing_source": 1, "not_eligible": 1})
        self.assertEqual(len(document["rows"]), 4)  # 2 eligible records x 2 models
        row = next(r for r in document["rows"] if r["record_id"] == "a" and r["model"] == "groq:qwen/qwen3.8-27b")
        self.assertEqual((row["status"], row["text"], row["raw_status"], row["http_status"]), ("ok", "alpha one fixed", "accepted", 200))
        self.assertEqual((row["prompt_tokens"], row["completion_tokens"], row["reasoning_tokens"]), (50, 12, 4))
        self.assertGreaterEqual(row["latency_ms"], 0)
        bodies = {r["body"]["model"]: r["body"] for r in self.provider.requests}
        self.assertEqual(set(bodies), {"qwen/qwen3.8-27b", "openai/gpt-oss-120b"})
        self.assertEqual(bodies["qwen/qwen3.8-27b"]["sentinel"], "from-rust")
        self.assertEqual(bodies["qwen/qwen3.8-27b"]["reasoning_effort"], "none")  # production value untouched
        self.assertEqual(bodies["openai/gpt-oss-120b"]["reasoning_effort"], "low")  # gpt-oss rejects "none"
        self.assertEqual(self.provider.requests[0]["auth"], f"Bearer {SECRET}")

    def test_429_honours_retry_after_and_caps_it(self):
        self.provider.script = {"alpha one": [("429", "3"), ("429", "999"), ("ok", "alpha one fixed")]}
        self.assertEqual(self.run_cli("--limit", "1"), 0, self.last_output)
        (row,) = self.rows()
        self.assertEqual((row["status"], row["retries"]), ("ok", 2))
        self.assertEqual(self.sleeps, [3.0, 60.0])

    def test_429_gives_up_after_three_retries(self):
        self.provider.script = {"alpha one": [("429", "1")] * 4}
        self.assertEqual(self.run_cli("--limit", "1"), 0, self.last_output)
        (row,) = self.rows()
        self.assertEqual((row["status"], row["raw_status"], row["retries"]), ("error", "http_429", 3))
        self.assertEqual(len(self.sleeps), 3)

    def test_http_error_is_recorded_and_never_leaks_the_key(self):
        self.provider.script = {"alpha one": [("500",)]}
        self.assertEqual(self.run_cli("--limit", "1"), 0, self.last_output)
        (row,) = self.rows()
        self.assertEqual((row["status"], row["http_status"], row["raw_status"]), ("error", 500, "http_500"))
        self.assertIsNone(row["text"])
        self.assertNotIn(SECRET, self.output.read_text())
        self.assertNotIn(SECRET, self.last_output)

    def test_timeout_is_an_error_row(self):
        self.provider.script = {"alpha one": [("hang",)]}
        self.assertEqual(self.run_cli("--limit", "1", "--timeout-seconds", "0.3"), 0, self.last_output)
        (row,) = self.rows()
        self.assertEqual((row["status"], row["raw_status"]), ("error", "timeout"))
        self.assertIsNone(row["latency_ms"])

    def test_resume_skips_ok_rows_and_retries_errors(self):
        self.provider.script = {"alpha one": [("ok", "alpha fixed")], "bravo one": [("500",)]}
        self.assertEqual(self.run_cli(), 0, self.last_output)
        self.assertEqual(len(self.provider.requests), 2)
        self.provider.script = {"bravo one": [("ok", "bravo fixed")]}
        self.assertEqual(self.run_cli(), 0, self.last_output)
        self.assertEqual(len(self.provider.requests), 3)  # only b was sent again
        by_record = {row["record_id"]: row for row in self.rows()}
        self.assertEqual((by_record["a"]["text"], by_record["b"]["text"]), ("alpha fixed", "bravo fixed"))
        self.assertTrue(all(row["status"] == "ok" for row in by_record.values()))

    def test_resume_refuses_a_different_configuration(self):
        self.assertEqual(self.run_cli(), 0, self.last_output)
        other = ["run", "--corpus", str(self.corpus), "--stt-run", str(self.stt), "--task", "intent-reconstruction",
                 "--source-a", "gq", "--source-b", "dg", "--output", str(self.output), "--rust-cmd", self.rust_cmd,
                 "--model", "groq:qwen/qwen3.8-27b"]
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(llm_bench.main(other), 2)

    def test_missing_key_fails_before_any_work(self):
        with mock.patch.dict(os.environ):
            os.environ.pop("GROQ_API_KEY")
            self.assertEqual(self.run_cli(), 2)
        self.assertIn("GROQ_API_KEY", self.last_output)
        self.assertFalse(self.output.exists())
        self.assertEqual(self.provider.requests, [])

    def test_key_is_absent_from_every_output(self):
        self.provider.script = {"alpha one": [("ok", "alpha fixed")], "bravo one": [("500",)]}
        self.assertEqual(self.run_cli(), 0, self.last_output)
        self.assertNotIn(SECRET, self.output.read_text())
        self.assertNotIn(SECRET, self.last_output)
        self.assertEqual({r["agent"] for r in self.provider.requests}, {llm_bench.USER_AGENT})

    def test_refuses_a_non_ignored_path_inside_a_git_work_tree(self):
        repo = self.dir / "repo"
        repo.mkdir()
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        tracked_output = repo / "run.json"
        self.assertEqual(self.run_cli(output=tracked_output), 2)
        self.assertIn("git-ignored", self.last_output)
        self.assertFalse(tracked_output.exists())
        (repo / ".gitignore").write_text("run.json\n", encoding="utf-8")
        self.assertEqual(self.run_cli(output=tracked_output), 0, self.last_output)
        self.assertTrue(tracked_output.exists())

    def test_refuses_an_ignored_but_tracked_file(self):
        repo = self.dir / "repo2"
        repo.mkdir()
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        target = repo / "run.json"
        target.write_text("{}", encoding="utf-8")
        subprocess.run(["git", "-C", str(repo), "add", "-f", "run.json"], check=True)
        (repo / ".gitignore").write_text("run.json\n", encoding="utf-8")
        self.assertEqual(self.run_cli(output=target), 2)

    def test_grammar_task_needs_only_source_a(self):
        argv = ["run", "--corpus", str(self.corpus), "--stt-run", str(self.stt), "--task", "grammar",
                "--source-a", "dg", "--output", str(self.output), "--rust-cmd", self.rust_cmd,
                "--model", "groq:openai/gpt-oss-20b", "--split", "development", "--limit", "1"]
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            code = llm_bench.main(argv)
        self.assertEqual(code, 0, out.getvalue())
        document = json.loads(self.output.read_text())
        self.assertEqual(document["task"], "grammar")
        self.assertEqual([row["status"] for row in document["rows"]], ["ok"])
        self.assertEqual(self.provider.requests[0]["body"]["model"], "openai/gpt-oss-20b")


if __name__ == "__main__":
    unittest.main()
