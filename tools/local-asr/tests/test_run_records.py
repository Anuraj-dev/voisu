import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest


ASR_ROOT = Path(__file__).parents[1]
TESTS = Path(__file__).parent
for path in (ASR_ROOT, TESTS):
    if str(path) not in sys.path:
        sys.path.insert(0, str(path))
SPEC = importlib.util.spec_from_file_location("voisu_local_asr_bench_records", ASR_ROOT / "bench.py")
bench = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(bench)
benchmark = bench.benchmark
run_records = bench.run_records

from test_bench import FAKE_CLI, make_wav  # noqa: E402  (shared fixtures, no test classes)

REFS = {
    "en-01.wav": {"language": "english", "reference": "ship the release", "split": "heldout", "critical_tokens": ["release"]},
    "en-02.wav": {"language": "english", "reference": "open the pull request now", "split": "heldout", "reference_source": "adjudicated"},
    "en-03.wav": {"language": "english", "reference": "tune on this one", "split": "development"},
    "hi-01.wav": {"language": "hinglish", "reference": "kal deploy karna"},
    "neg-01.wav": {"language": "negative", "reference": ""},
}


class RunRecordTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.eval_dir = self.root / "eval"
        audio = self.eval_dir / "audio"
        audio.mkdir(parents=True)
        for index, name in enumerate(REFS):
            make_wav(audio / name, 1600 + 200 * index)
        (self.eval_dir / "references.json").write_text(json.dumps(REFS), encoding="utf-8")
        (self.eval_dir / "consent.json").write_text(json.dumps({"consent_id": "c-1"}), encoding="utf-8")
        inventory = benchmark.inventory(audio, self.eval_dir / "references.json", self.eval_dir / "consent.json", "records-v1")
        self.corpus = benchmark.freeze(inventory, "seed", 0.5)
        benchmark.write_json(self.eval_dir / "corpus.json", self.corpus)
        self.cli = self.root / "fake-cli"
        self.cli.write_text(FAKE_CLI, encoding="utf-8")
        self.cli.chmod(0o755)

    def manifest(self, model_id, texts):
        # texts: record_id -> hypothesis; anything missing is transcribed perfectly.
        ordered = [texts.get(r["record_id"], r["reference"]) for r in self.corpus["records"]]
        path = self.root / f"{model_id}.json"
        path.write_text(json.dumps({"model_id": model_id, "fake_texts": ordered}), encoding="utf-8")
        return f"{model_id}={path}"

    def bench_run(self, models, **kwargs):
        run_dir, report, _ = bench.run_bench(self.eval_dir, self.eval_dir / "corpus.json", models, self.root, str(self.cli), 30, fixture=True, **kwargs)
        return run_dir, report, benchmark.read_json(run_dir / "run_records.json")

    def test_bench_run_writes_one_record_per_model_with_every_slot(self):
        run_dir, report, result = self.bench_run([self.manifest("model-a", {}), self.manifest("model-b", {"en-02.wav": "open the request now", "neg-01.wav": "thank you"})])
        self.assertEqual(result["kind"], "run_report")
        self.assertEqual(result["corpus_manifest_sha256"], self.corpus["manifest_sha256"])
        self.assertEqual([r["run_key"]["model_id"] for r in result["records"]], ["model-a", "model-b"])
        for record in result["records"]:
            self.assertEqual(tuple(record), run_records.RUN_RECORD_FIELDS)
            self.assertEqual(set(record["run_key"]), {"model_id", "runtime", "backend", "device", "threads", "window", "host_profile"})
            self.assertEqual(record["run_key"]["window"], {"window_s": 30, "overlap_s": 0, "merge": "none", "source": run_records.WORKER_DEFAULT_WINDOW["source"]})
            self.assertEqual(set(record["scopes"]), set(run_records.SCOPES))
            for scope in record["scopes"].values():
                self.assertEqual(tuple(scope), run_records.SCOPE_FIELDS)
                for metric in run_records.METRICS:
                    self.assertEqual(tuple(scope[metric]), run_records.METRIC_FIELDS)
                    self.assertEqual(tuple(scope[metric]["ci"]), run_records.CI_FIELDS)
            self.assertEqual(set(record["latency_ms"]), {"warm_inference", "stop_to_transcript"})
            self.assertEqual(set(record["memory"]), {"peak_rss_bytes", "peak_gpu_memory_bytes", "system_memory_delta_bytes"})
            self.assertEqual(set(record["negatives"]), {"clips", "insertions", "clips_with_insertions"})
        # No transcript text in the records file.
        self.assertNotIn("open the request now", (run_dir / "run_records.json").read_text())
        self.assertNotIn("thank you", (run_dir / "run_records.json").read_text())

    def test_strict_wer_and_latency_match_the_score_report(self):
        _, report, result = self.bench_run([self.manifest("model-a", {"en-01.wav": "ship release", "en-03.wav": "tune on this one too"}), self.manifest("model-b", {})])
        for model, record in zip(report["models"], result["records"]):
            everything = record["scopes"]["english_all"]
            self.assertEqual(everything["strict_wer"]["status"], "measured")
            self.assertEqual(everything["strict_wer"]["estimate"], model["languages"]["english"]["raw_strict"]["wer"])
            self.assertEqual(everything["clips"], {"measured": 3, "expected": 3})
            warm = model["timings"]["warm_inference_ms"]
            self.assertEqual(record["latency_ms"]["warm_inference"], {"p50": warm["p50_ms"], "p95": warm["p95_ms"], "samples": warm["sample_count"]})
            self.assertEqual(record["memory"]["peak_rss_bytes"], model["resources"]["peak_host_ram_bytes"])
        a = result["records"][0]["scopes"]
        self.assertEqual(a["english_all"]["strict_wer"]["numerator"], 2)  # one deletion, one insertion
        self.assertEqual(a["english_all"]["strict_wer"]["denominator"], 12)
        # The decision scope holds held-out English only.
        self.assertEqual(a["english_heldout"]["clips"], {"measured": 2, "expected": 2})
        self.assertEqual([item[0] for item in a["english_heldout"]["strict_wer"]["per_clip"]], ["en-01.wav", "en-02.wav"])
        self.assertAlmostEqual(a["english_heldout"]["strict_wer"]["estimate"], 1 / 8)

    def test_metric_slots_hold_real_values(self):
        _, _, result = self.bench_run([self.manifest("model-a", {"en-02.wav": "open the request"})])
        record = result["records"][0]
        heldout = record["scopes"]["english_heldout"]
        self.assertEqual(heldout["normalized_wer"]["status"], "measured")
        self.assertEqual(heldout["normalized_wer"]["estimate"], 2 / 8)
        self.assertEqual(heldout["critical_token_error_rate"]["status"], "measured")
        self.assertEqual(heldout["critical_token_error_rate"]["per_clip"], [["en-01.wav", 0, 1]])  # only en-01 lists critical tokens
        self.assertEqual(heldout["strict_wer"]["ci"]["status"], "measured")
        self.assertLessEqual(heldout["strict_wer"]["ci"]["low"], heldout["strict_wer"]["estimate"])
        self.assertGreaterEqual(heldout["strict_wer"]["ci"]["high"], heldout["strict_wer"]["estimate"])
        self.assertIsNone(record["memory"]["system_memory_delta_bytes"])
        self.assertTrue(any("system-wide memory" in item for item in record["limitations"]))
        self.assertNotIn("CI n/i", run_records.format_run_report(result))

    def test_negative_insertions_are_counted(self):
        _, _, result = self.bench_run([self.manifest("model-a", {"neg-01.wav": "thank you for watching"}), self.manifest("model-b", {})])
        self.assertEqual(result["records"][0]["negatives"], {"clips": 1, "insertions": 4, "clips_with_insertions": 1})
        self.assertEqual(result["records"][1]["negatives"], {"clips": 1, "insertions": 0, "clips_with_insertions": 0})

    def test_comparisons_pair_every_two_records_and_verdicts_follow_the_rule(self):
        _, _, result = self.bench_run([self.manifest("model-a", {"en-02.wav": "open the request"}), self.manifest("model-b", {})], baseline_id="model-a")
        self.assertEqual(result["incumbent"], "model-a")
        self.assertEqual(len(result["comparisons"]), len(run_records.SCOPES) * len(run_records.METRICS))
        strict = next(c for c in result["comparisons"] if c["scope"] == "english_heldout" and c["metric"] == "strict_wer")
        self.assertEqual(strict["status"], "measured")
        self.assertEqual(strict["clips"], 2)
        self.assertAlmostEqual(strict["difference"]["estimate"], 0 - 2 / 8)
        self.assertLessEqual(strict["difference"]["low"], strict["difference"]["estimate"])
        self.assertEqual(len(result["verdicts"]), 1)
        verdict = result["verdicts"][0]
        # Two held-out clips cannot separate the models: the paired CI reaches 0, so no winner.
        self.assertEqual(verdict["verdict"], "not_winner")
        self.assertEqual(verdict["checks"], {"paired_ci_better": False, "zero_negative_insertions": True, "no_critical_regression": True})

    def test_records_from_different_corpora_are_not_paired(self):
        _, _, result = self.bench_run([self.manifest("model-a", {}), self.manifest("model-b", {})])
        a, b = result["records"]
        with self.assertRaises(ValueError):
            run_records.compare(a, {**b, "corpus_manifest_sha256": "0" * 64}, "english_heldout", "strict_wer")

    def test_comparison_drops_clips_one_side_lacks(self):
        _, _, result = self.bench_run([self.manifest("model-a", {}), self.manifest("model-b", {})])
        a, b = result["records"]
        trimmed = json.loads(json.dumps(b))
        trimmed["scopes"]["english_heldout"]["strict_wer"]["per_clip"] = trimmed["scopes"]["english_heldout"]["strict_wer"]["per_clip"][:1]
        item = run_records.compare(a, trimmed, "english_heldout", "strict_wer")
        self.assertEqual((item["clips"], item["dropped_clips"]), (1, 1))

    def test_records_cli_writes_the_report(self):
        run_dir, _, _ = self.bench_run([self.manifest("model-a", {}), self.manifest("model-b", {})])
        output = self.root / "records.json"
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            code = benchmark.main(["records", "--corpus", str(self.eval_dir / "corpus.json"), "--evidence", str(run_dir / "replay.json"),
                                   "--report", str(run_dir / "report.json"), "--threads", "model-a=8", "--window-s", "20", "--overlap-s", "2",
                                   "--incumbent", "model-a", "--output", str(output)])
        self.assertEqual(code, 0)
        result = benchmark.read_json(output)
        key = result["records"][0]["run_key"]
        self.assertEqual((key["threads"], key["window"]["window_s"], key["window"]["overlap_s"], key["window"]["source"]), (8, 20, 2, "declared"))
        self.assertIsNone(result["records"][1]["run_key"]["threads"])
        self.assertIn("verdict model-b", stdout.getvalue())


def _comparison(metric, *, estimate, low=None, high=None, status="measured", baseline="inc", candidate="cand"):
    return {"baseline": baseline, "candidate": candidate, "scope": run_records.DECISION_SCOPE, "metric": metric, "status": status,
            "difference": {"estimate": estimate, "low": low, "high": high, "excludes_zero": None if low is None else (low > 0 or high < 0)}}


class WinnerRuleTests(unittest.TestCase):
    incumbent = {"label": "inc", "negatives": {"insertions": 0}}

    def verdict(self, primary, critical, insertions=0):
        candidate = {"label": "cand", "negatives": {"insertions": insertions}}
        return run_records.winner_verdict(self.incumbent, candidate, [primary, critical])

    def test_winner_needs_all_three_checks(self):
        better = _comparison(run_records.PRIMARY_METRIC, estimate=-0.02, low=-0.03, high=-0.01)
        no_regression = _comparison("critical_token_error_rate", estimate=0.0, low=-0.01, high=0.01)
        self.assertEqual(self.verdict(better, no_regression)["verdict"], "winner")

    def test_any_failed_check_is_not_winner(self):
        better = _comparison(run_records.PRIMARY_METRIC, estimate=-0.02, low=-0.03, high=-0.01)
        overlaps = _comparison(run_records.PRIMARY_METRIC, estimate=-0.02, low=-0.03, high=0.001)
        fine = _comparison("critical_token_error_rate", estimate=0.0)
        worse = _comparison("critical_token_error_rate", estimate=0.01)
        self.assertEqual(self.verdict(overlaps, fine)["verdict"], "not_winner")
        self.assertEqual(self.verdict(better, worse)["verdict"], "not_winner")
        self.assertEqual(self.verdict(better, fine, insertions=1)["verdict"], "not_winner")
        # A failed check decides even when another is still unmeasured.
        self.assertEqual(self.verdict(_comparison(run_records.PRIMARY_METRIC, estimate=None, status="not_implemented"), worse)["verdict"], "not_winner")

    def test_unmeasured_check_is_pending(self):
        better = _comparison(run_records.PRIMARY_METRIC, estimate=-0.02, low=-0.03, high=-0.01)
        unmeasured = _comparison("critical_token_error_rate", estimate=None, status="not_implemented")
        result = self.verdict(better, unmeasured)
        self.assertEqual(result["verdict"], "pending")
        self.assertIsNone(result["checks"]["no_critical_regression"])

    def test_comparisons_for_other_scopes_or_pairs_are_ignored(self):
        better = _comparison(run_records.PRIMARY_METRIC, estimate=-0.02, low=-0.03, high=-0.01)
        other_scope = {**better, "scope": "english_all"}
        other_pair = {**better, "candidate": "someone-else"}
        candidate = {"label": "cand", "negatives": {"insertions": 0}}
        self.assertEqual(run_records.winner_verdict(self.incumbent, candidate, [other_scope, other_pair])["checks"]["paired_ci_better"], None)


if __name__ == "__main__":
    unittest.main()
