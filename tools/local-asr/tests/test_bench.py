import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
import wave


ASR_ROOT = Path(__file__).parents[1]
if str(ASR_ROOT) not in sys.path:
    sys.path.insert(0, str(ASR_ROOT))
SPEC = importlib.util.spec_from_file_location("voisu_local_asr_bench", ASR_ROOT / "bench.py")
bench = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(bench)
benchmark = bench.benchmark

NOW_MS = 1_800_000_000_000
DAY_MS = 86_400_000


def make_wav(path: Path, frames: int = 1600) -> None:
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(16_000)
        handle.writeframes(b"\x01\0" * frames)


def make_eval_dir(root: Path) -> Path:
    """A synthetic frozen corpus in the layout of the real evaluation directory."""

    eval_dir = root / "eval"
    audio = eval_dir / "audio"
    audio.mkdir(parents=True)
    make_wav(audio / "en-01.wav")
    make_wav(audio / "hi-01.wav", 2400)
    make_wav(audio / "neg-01.wav", 800)
    refs = {
        "en-01.wav": {"language": "english", "reference": "ship the release", "tags": ["commands"], "critical_tokens": ["release"]},
        "hi-01.wav": {"language": "hinglish", "reference": "kal deploy karna"},
        "neg-01.wav": {"language": "negative", "reference": ""},
    }
    (eval_dir / "references.json").write_text(json.dumps(refs), encoding="utf-8")
    (eval_dir / "references.draft.json").write_text(json.dumps(refs, indent=2) + "\n", encoding="utf-8")
    (eval_dir / "consent.json").write_text(json.dumps({"consent_id": "c-1"}), encoding="utf-8")
    inventory = benchmark.inventory(audio, eval_dir / "references.json", eval_dir / "consent.json", "synthetic-v1")
    corpus = benchmark.freeze(inventory, "seed", 0.34)
    benchmark.write_json(eval_dir / "inventory.json", inventory)
    benchmark.write_json(eval_dir / "corpus.json", corpus)
    return eval_dir


def make_diagnostics(root: Path, captures: list[dict]) -> Path:
    """captures: dicts with id, pcm (bytes or None for pruned), transcript, age_days."""

    diagnostics = root / "diagnostics"
    (diagnostics / "audio").mkdir(parents=True)
    lines = []
    for item in captures:
        captured = NOW_MS - int(item.get("age_days", 1) * DAY_MS)
        name = f"{item['id']}-exp{captured + 7 * DAY_MS}.pcm"
        if item.get("pcm") is not None:
            (diagnostics / "audio" / name).write_bytes(item["pcm"])
        row = {"correlation_id": item["id"], "recorded_at_unix_ms": captured, "debug_audio": {"file_name": name, "captured_at_unix_ms": captured, "expires_at_unix_ms": captured + 7 * DAY_MS}}
        if item.get("transcript") is not None:
            row["final_transcript"] = item["transcript"]
        if item.get("no_audio"):
            row["debug_audio"] = None
        lines.append(json.dumps(row))
    lines.insert(1, "{not json")
    (diagnostics / "history.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return diagnostics


def pcm(seed: int, frames: int = 8000) -> bytes:
    return bytes((seed + i) % 251 + 1 for i in range(frames * 2))


class AddCapturesTests(unittest.TestCase):
    def test_adds_unreviewed_entries_and_skips_unusable_captures_with_reasons(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            eval_dir = make_eval_dir(root)
            diagnostics = make_diagnostics(root, [
                {"id": "rec-1-1", "pcm": pcm(1), "transcript": "first delivered text"},
                {"id": "rec-1-2", "pcm": pcm(1), "transcript": "same audio again"},
                {"id": "rec-1-3", "pcm": pcm(3), "transcript": None},
                {"id": "rec-1-4", "pcm": None, "transcript": "audio pruned"},
                {"id": "rec-1-5", "pcm": pcm(5), "transcript": "too old", "age_days": 30},
                {"id": "rec-1-6", "pcm": b"\0" * 16_000, "transcript": "silent"},
                {"id": "rec-1-7", "pcm": pcm(7, 100), "transcript": "too short"},
                {"id": "rec-1-8", "pcm": pcm(8), "transcript": "no capture recorded", "no_audio": True},
                {"id": "rec-1-9", "pcm": pcm(9), "transcript": "second delivered text"},
            ])
            before_audio = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in (eval_dir / "audio").iterdir()}
            before_refs = json.loads((eval_dir / "references.draft.json").read_text())
            corpus_bytes = (eval_dir / "corpus.json").read_bytes()

            result = bench.add_captures(eval_dir, diagnostics, 7, now_ms=NOW_MS)

            self.assertEqual(result["added"], ["cap-rec-1-1.wav", "cap-rec-1-9.wav"])
            self.assertEqual(result["skipped"], {
                "duplicate audio (already in corpus or captures)": 1,
                "no delivered transcript (failed or empty dictation)": 1,
                "audio file already pruned": 1,
                "older than --since window": 1,
                "silent audio": 1,
                "shorter than 300 ms": 1,
                "no debug audio recorded": 1,
                "unreadable history line": 1,
            })
            refs = json.loads((eval_dir / "references.draft.json").read_text())
            for key, value in before_refs.items():
                self.assertEqual(refs[key], value)
            entry = refs["cap-rec-1-1.wav"]
            self.assertEqual(entry["reference"], "first delivered text")
            self.assertEqual(entry["review_status"], "unreviewed")
            self.assertEqual(entry["language"], bench.UNREVIEWED_LANGUAGE)
            self.assertEqual(result["unreviewed_total"], 2)
            wav = eval_dir / "audio" / "cap-rec-1-1.wav"
            self.assertEqual(benchmark.wav_info(wav), (500, 16_000))
            self.assertEqual(stat.S_IMODE(wav.stat().st_mode), 0o600)
            for name, digest in before_audio.items():
                self.assertEqual(hashlib.sha256((eval_dir / "audio" / name).read_bytes()).hexdigest(), digest)
            self.assertEqual((eval_dir / "corpus.json").read_bytes(), corpus_bytes)

    def test_rerun_adds_nothing_and_unreviewed_entries_block_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            eval_dir = make_eval_dir(root)
            diagnostics = make_diagnostics(root, [{"id": "rec-2-1", "pcm": pcm(2), "transcript": "hello there"}])
            self.assertEqual(len(bench.add_captures(eval_dir, diagnostics, 7, now_ms=NOW_MS)["added"]), 1)
            again = bench.add_captures(eval_dir, diagnostics, 7, now_ms=NOW_MS)
            self.assertEqual(again["added"], [])
            self.assertEqual(again["skipped"]["duplicate audio (already in corpus or captures)"], 1)
            with self.assertRaises(benchmark.BenchmarkError):
                benchmark.inventory(eval_dir / "audio", eval_dir / "references.draft.json", eval_dir / "consent.json", "synthetic-v2")
            refs = json.loads((eval_dir / "references.draft.json").read_text())
            refs["cap-rec-2-1.wav"].update(language="english", reference="hello there friend", review_status="reviewed")
            (eval_dir / "references.draft.json").write_text(json.dumps(refs), encoding="utf-8")
            inventory = benchmark.inventory(eval_dir / "audio", eval_dir / "references.draft.json", eval_dir / "consent.json", "synthetic-v2")
            self.assertEqual(len(inventory["records"]), 4)

    def test_dry_run_and_existing_name_with_other_content_write_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            eval_dir = make_eval_dir(root)
            diagnostics = make_diagnostics(root, [{"id": "rec-3-1", "pcm": pcm(3), "transcript": "dry"}, {"id": "rec-3-2", "pcm": pcm(4), "transcript": "collides"}])
            existing = eval_dir / "audio" / "cap-rec-3-2.wav"
            make_wav(existing, 4000)
            kept = existing.read_bytes()
            refs_before = (eval_dir / "references.draft.json").read_bytes()
            result = bench.add_captures(eval_dir, diagnostics, 7, dry_run=True, now_ms=NOW_MS)
            self.assertEqual(result["added"], ["cap-rec-3-1.wav"])
            self.assertEqual(result["skipped"], {"name already used by different content (kept existing)": 1, "unreadable history line": 1})
            self.assertFalse((eval_dir / "audio" / "cap-rec-3-1.wav").exists())
            self.assertEqual((eval_dir / "references.draft.json").read_bytes(), refs_before)
            bench.add_captures(eval_dir, diagnostics, 7, now_ms=NOW_MS)
            self.assertEqual(existing.read_bytes(), kept)

    def test_orphan_wav_from_a_died_run_gets_its_reference_added(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            eval_dir = make_eval_dir(root)
            diagnostics = make_diagnostics(root, [{"id": "rec-4-1", "pcm": pcm(11), "transcript": "orphaned text"}, {"id": "rec-4-2", "pcm": pcm(12), "transcript": "fresh text"}])
            # The earlier run wrote the WAV, then died before saving references.draft.json.
            orphan = eval_dir / "audio" / "cap-rec-4-1.wav"
            orphan.write_bytes(bench.pcm_to_wav(pcm(11)))
            result = bench.add_captures(eval_dir, diagnostics, 7, now_ms=NOW_MS)
            self.assertEqual(result["added"], ["cap-rec-4-1.wav", "cap-rec-4-2.wav"])
            self.assertNotIn("duplicate audio (already in corpus or captures)", result["skipped"])
            refs = json.loads((eval_dir / "references.draft.json").read_text())
            self.assertEqual(refs["cap-rec-4-1.wav"]["reference"], "orphaned text")
            self.assertEqual(refs["cap-rec-4-1.wav"]["review_status"], "unreviewed")
            self.assertEqual(orphan.read_bytes(), bench.pcm_to_wav(pcm(11)))
            self.assertEqual(bench.add_captures(eval_dir, diagnostics, 7, now_ms=NOW_MS)["added"], [])
            # A same-named file with other content is still left alone.
            diagnostics = make_diagnostics(root / "again", [{"id": "rec-4-3", "pcm": pcm(13), "transcript": "x"}])
            clash = eval_dir / "audio" / "cap-rec-4-3.wav"
            make_wav(clash, 4000)
            kept = clash.read_bytes()
            self.assertEqual(bench.add_captures(eval_dir, diagnostics, 7, now_ms=NOW_MS)["added"], [])
            self.assertEqual(clash.read_bytes(), kept)

    def test_inventory_refuses_a_draft_whose_language_was_set_but_review_status_was_not(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            eval_dir = make_eval_dir(root)
            diagnostics = make_diagnostics(root, [{"id": "rec-5-1", "pcm": pcm(21), "transcript": "hello there"}])
            bench.add_captures(eval_dir, diagnostics, 7, now_ms=NOW_MS)
            draft = eval_dir / "references.draft.json"
            refs = json.loads(draft.read_text())
            refs["cap-rec-5-1.wav"].update(language="english", reference="hello there friend")  # review_status stays "unreviewed"
            draft.write_text(json.dumps(refs), encoding="utf-8")
            with self.assertRaisesRegex(benchmark.BenchmarkError, 'set review_status to "reviewed"'):
                benchmark.inventory(eval_dir / "audio", draft, eval_dir / "consent.json", "synthetic-v3")
            refs["cap-rec-5-1.wav"]["review_status"] = "reviewed"
            draft.write_text(json.dumps(refs), encoding="utf-8")
            self.assertEqual(len(benchmark.inventory(eval_dir / "audio", draft, eval_dir / "consent.json", "synthetic-v3")["records"]), 4)

    def test_add_captures_output_says_how_to_mark_a_draft_reviewed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            eval_dir = make_eval_dir(root)
            diagnostics = make_diagnostics(root, [{"id": "rec-6-1", "pcm": pcm(31), "transcript": "hello"}])
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                self.assertEqual(bench.main(["add-captures", "--eval-dir", str(eval_dir), "--diagnostics-dir", str(diagnostics), "--since", "100000"]), 0)
            self.assertIn('set review_status to "reviewed" and remove the "unreviewed" tag', stdout.getvalue())

    def test_next_steps_reuse_corpus_seed_and_never_overwrite_the_frozen_corpus(self):
        with tempfile.TemporaryDirectory() as directory:
            eval_dir = make_eval_dir(Path(directory))
            text = bench.next_steps(eval_dir, "20260101T000000Z")
            self.assertIn("inventory --input-dir", text)
            self.assertIn("--seed seed", text)
            self.assertIn("--corpus-id synthetic-v1", text)
            self.assertIn("inventory-20260101T000000Z.json", text)
            self.assertIn("corpus-20260101T000000Z.json", text)
            self.assertNotIn(f"--output {eval_dir / 'corpus.json'}", text)


class ModelDiscoveryTests(unittest.TestCase):
    def test_lists_ready_and_missing_models_and_resolves_bare_ids(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            weights = root / "weights"
            weights.mkdir()
            manifests = root / "local-dev" / "manifests"
            manifests.mkdir(parents=True)
            (manifests / "qwen3-asr-0.6b.json").write_text(json.dumps({"model_id": "qwen3-asr-0.6b", "device": "cuda:0", "model_path": str(weights), "files": []}), encoding="utf-8")
            (manifests / "qwen3-asr-1.7b.json").write_text(json.dumps({"model_id": "qwen3-asr-1.7b", "device": "cuda:0", "model_path": str(root / "gone"), "runtime": {"python": str(root / "no-python")}, "files": [{"path": str(root / "gone.bin")}]}), encoding="utf-8")
            (manifests / "qwen3-asr-0.6b.fingerprints.json").write_text("{}", encoding="utf-8")
            (manifests / "qwen3-asr-1.7b.fingerprints.json").write_text("{}", encoding="utf-8")
            (manifests / "broken.json").write_text("{", encoding="utf-8")
            found = bench.discover_models(root)
            self.assertEqual([(m["model_id"], m["ready"]) for m in found], [("qwen3-asr-0.6b", True), ("qwen3-asr-1.7b", False)])
            self.assertEqual(found[1]["missing"], ["model_path", "runtime.python", "1 model file(s)"])
            self.assertEqual(bench.resolve_models(["qwen3-asr-0.6b"], root), [f"qwen3-asr-0.6b={manifests / 'qwen3-asr-0.6b.json'}"])
            with self.assertRaisesRegex(benchmark.BenchmarkError, "not ready"):
                bench.resolve_models(["qwen3-asr-1.7b"], root)
            with self.assertRaisesRegex(benchmark.BenchmarkError, "no manifest found"):
                bench.resolve_models(["nemotron-3.5-0.6b"], root)
            pinned = root / "worker.py"
            pinned.write_text("changed", encoding="utf-8")
            (manifests / "parakeet-tdt-0.6b-v3.json").write_text(json.dumps({"model_id": "parakeet-tdt-0.6b-v3", "files": [{"path": str(pinned), "bytes": 999}]}), encoding="utf-8")
            (manifests / "parakeet-tdt-0.6b-v3.fingerprints.json").write_text("{}", encoding="utf-8")
            stale = {m["model_id"]: m for m in bench.discover_models(root)}["parakeet-tdt-0.6b-v3"]
            self.assertFalse(stale["ready"])
            self.assertEqual(stale["missing"], ["1 stale pin(s): regenerate the manifest"])
            self.assertEqual(bench.resolve_models([f"x-model={manifests}/a.json"], root), [f"x-model={manifests}/a.json"])

    def test_ready_matches_what_replay_loads(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            registered = root / "local-dev" / "manifests"
            private = root / "private-manifests"
            registered.mkdir(parents=True)
            private.mkdir()
            body = json.dumps({"model_id": "qwen3-asr-0.6b", "device": "cpu"})
            # A private manifest alone is not registered, so replay would refuse it.
            (private / "qwen3-asr-0.6b.json").write_text(body, encoding="utf-8")
            only_private = bench.discover_models(root)[0]
            self.assertFalse(only_private["ready"])
            self.assertIn("not registered", only_private["missing"][0])
            # Registered but without the fingerprints file: not ready.
            (registered / "qwen3-asr-0.6b.json").write_text(body, encoding="utf-8")
            (registered / "not-in-catalog.json").write_text(json.dumps({"model_id": "not-in-catalog"}), encoding="utf-8")
            (registered / "not-in-catalog.fingerprints.json").write_text("{}", encoding="utf-8")
            (registered / "renamed.json").write_text(json.dumps({"model_id": "qwen3-asr-1.7b"}), encoding="utf-8")
            (registered / "renamed.fingerprints.json").write_text("{}", encoding="utf-8")
            found = {m["model_id"]: m for m in bench.discover_models(root)}
            self.assertEqual(found["qwen3-asr-0.6b"]["missing"], ["no fingerprints file"])
            self.assertEqual(found["not-in-catalog"]["missing"], ["not in the development catalog"])
            self.assertEqual(found["qwen3-asr-1.7b"]["missing"], ["file name differs from model_id"])
            # Registered with fingerprints: ready, and the registered copy wins over the private one.
            (registered / "qwen3-asr-0.6b.fingerprints.json").write_text("{}", encoding="utf-8")
            found = {m["model_id"]: m for m in bench.discover_models(root)}
            self.assertTrue(found["qwen3-asr-0.6b"]["ready"])
            self.assertEqual(found["qwen3-asr-0.6b"]["path"], str(registered / "qwen3-asr-0.6b.json"))

    def test_catalog_ids_come_from_the_rust_source(self):
        ids = bench.catalog_ids()
        self.assertIsNotNone(ids)
        self.assertIn("qwen3-asr-0.6b", ids)


FAKE_CLI = """#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
audios = [args[i + 1] for i, a in enumerate(args) if a == "--audio"]
texts = json.load(open(args[args.index("--manifest") + 1]))["fake_texts"]
for index, audio in enumerate(audios):
    print(json.dumps({"status": "transcript", "text": texts[index], "phase": "cold" if index == 0 else "warm", "resident": index > 0, "load_ms": 50 if index == 0 else None, "inference_ms": 10 + index, "duration_profile": "short"}))
"""


class RunTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.eval_dir = make_eval_dir(self.root)
        self.cli = self.root / "fake-cli"
        self.cli.write_text(FAKE_CLI, encoding="utf-8")
        self.cli.chmod(0o755)
        corpus = benchmark.read_json(self.eval_dir / "corpus.json")
        self.order = [r["record_id"] for r in corpus["records"]]
        self.truth = {r["record_id"]: r["reference"] for r in corpus["records"]}

    def manifest(self, model_id, wrong_words):
        # Texts in corpus order; the first speech clip gets `wrong_words` words dropped.
        texts, dropped = [], False
        for record_id in self.order:
            words = self.truth[record_id].split()
            if words and not dropped:
                words, dropped = words[wrong_words:], True
            texts.append(" ".join(words))
        path = self.root / f"{model_id}.json"
        path.write_text(json.dumps({"model_id": model_id, "fake_texts": texts}), encoding="utf-8")
        return f"{model_id}={path}"

    def run_bench(self, models, **kwargs):
        return bench.run_bench(self.eval_dir, self.eval_dir / "corpus.json", models, self.root, str(self.cli), 30, fixture=True, **kwargs)

    def test_run_writes_a_timestamped_folder_with_table_and_metrics(self):
        run_dir, report, table = self.run_bench([self.manifest("model-a", 0), self.manifest("model-b", 1)])
        self.assertEqual(run_dir.parent, self.eval_dir / "runs")
        for name in ("replay.json", "report.json", "run.json", "table.txt"):
            self.assertTrue((run_dir / name).is_file(), name)
        self.assertEqual(sorted(m["model_id"] for m in report["models"]), ["model-a", "model-b"])
        lines = table.splitlines()
        self.assertTrue(lines[0].startswith("model"))
        header = [cell for cell in lines[0].split("  ") if cell.strip()]
        raw_column = header.index("raw WER")
        rows = {line.split()[0]: [cell for cell in line.split("  ") if cell.strip()] for line in lines[1:3]}
        self.assertEqual(rows["model-a"][raw_column], "0.0%")
        self.assertNotEqual(rows["model-b"][raw_column], "0.0%")  # model-b dropped a word
        self.assertIn("FIXTURE", table)
        self.assertNotIn("ship the release", table)  # transcript text never goes to the table
        self.assertEqual((run_dir / "table.txt").read_text().strip(), table.strip())

    def test_second_run_is_compared_with_the_earlier_one(self):
        first, _, first_table = self.run_bench([self.manifest("model-a", 0)])
        self.assertNotIn("[base]", first_table)
        second, _, table = self.run_bench([self.manifest("model-a", 1), self.manifest("model-c", 0)], baseline_id="model-a")
        self.assertNotEqual(first, second)
        self.assertIn("[base] model-a", table)
        self.assertIn(f"runs/{first.name}/report.json", table)
        row = next(line for line in table.splitlines() if line.startswith("model-a "))
        self.assertIn("pp", row)
        self.assertIn("+", row)  # model-a got worse by one dropped word
        meta = json.loads((second / "run.json").read_text())
        self.assertEqual(meta["baseline"]["model_id"], "model-a")

    def test_default_baseline_is_the_model_with_most_runs_and_unknown_baseline_is_reported(self):
        self.run_bench([self.manifest("model-a", 0), self.manifest("model-b", 0)])
        self.run_bench([self.manifest("model-b", 0)])
        _, _, table = self.run_bench([self.manifest("model-b", 0)])
        self.assertIn("[base] model-b", table)
        _, _, table = self.run_bench([self.manifest("model-a", 0)], baseline_id="model-z")
        self.assertIn("no earlier full run of baseline model-z", table)
        self.assertNotIn("[base]", table)

    def test_reports_on_a_different_corpus_or_real_evidence_are_not_baselines(self):
        self.run_bench([self.manifest("model-a", 0)])
        history = bench.load_history(self.eval_dir)
        self.assertEqual(bench.pick_baseline(history, "0" * 64, "fixture", None), (None, None))
        self.assertEqual(bench.pick_baseline(history, history[0]["report"]["corpus_manifest_sha256"], "real", None), (None, None))

    def partial_manifest(self, model_id, keep):
        path = self.root / f"{model_id}-partial.json"
        texts = json.loads(Path(self.manifest(model_id, 0).split("=", 1)[1]).read_text())["fake_texts"][:keep]
        path.write_text(json.dumps({"model_id": model_id, "fake_texts": texts}), encoding="utf-8")
        return f"{model_id}={path}"

    def test_partial_runs_are_never_baselines_and_never_produce_deltas(self):
        full, _, _ = self.run_bench([self.manifest("model-a", 0)])
        _, report, table = self.run_bench([self.partial_manifest("model-a", 2)], baseline_id="model-a")
        self.assertEqual(report["models"][0]["status"], "measured_partial")
        self.assertIn(f"runs/{full.name}/report.json", table)  # compared against the full run only
        row = next(line for line in table.splitlines() if line.startswith("model-a "))
        self.assertNotIn("pp", row)  # 2 of 3 clips against 3 of 3 is not a like-for-like delta
        self.assertNotIn("d raw", table.splitlines()[0])
        # With the partial run in the history, a later full run still compares with the full one.
        _, _, again = self.run_bench([self.manifest("model-a", 1)], baseline_id="model-a")
        self.assertIn(f"runs/{full.name}/report.json", again)
        self.assertIn("(1 earlier run(s)", again)
        # A history that holds only a partial run offers no baseline at all.
        history = bench.load_history(self.eval_dir)
        only_partial = [entry for entry in history if any(m["status"] == "measured_partial" for m in entry["report"]["models"])]
        self.assertEqual(len(only_partial), 1)
        corpus_sha = only_partial[0]["report"]["corpus_manifest_sha256"]
        baseline, note = bench.pick_baseline(only_partial, corpus_sha, "fixture", "model-a")
        self.assertIsNone(baseline)
        self.assertIn("no earlier full run", note)

    def test_baseline_must_come_from_the_same_device_and_host(self):
        self.run_bench([self.manifest("model-a", 0)])
        history = bench.load_history(self.eval_dir)
        corpus_sha = history[0]["report"]["corpus_manifest_sha256"]
        gpu = {"requested_device": "cuda:0", "observed_device": "cuda:0", "host_profile": "unprofiled"}
        cpu = {"requested_device": "cpu", "observed_device": "cpu", "host_profile": "unprofiled"}
        history[0]["report"]["models"][0]["identity"] = gpu

        def pick(identity):
            return bench.pick_baseline(history, corpus_sha, "fixture", None, scopes={bench._scope({"identity": identity})})[0]

        self.assertIsNotNone(pick(gpu))
        self.assertIsNone(pick(cpu))
        self.assertIsNone(pick(dict(gpu, host_profile="laptop")))
        # A row on another device gets no delta even when a baseline was chosen.
        report = {"models": [{"model_id": "model-a", "status": "measured", "identity": cpu, "sample_count": 3, "expected_sample_count": 3,
                              "raw_strict": {"wer": 0.5}, "timings": {"warm_inference_ms": {"p50_ms": 5, "p95_ms": 6}}}]}
        baseline = {"model_id": "model-a", "label": "x", "runs": 1, "model": history[0]["report"]["models"][0]}
        row = next(line for line in bench.build_table(report, baseline).splitlines() if line.startswith("model-a "))
        self.assertNotIn("pp", row)
        self.assertNotIn("ms ", row.split("5/6")[-1])

    def test_unavailable_model_is_reported_without_fake_numbers_and_a_crashed_run_leaves_no_folder(self):
        broken = self.root / "broken.json"
        broken.write_text(json.dumps({"model_id": "model-a", "fake_texts": []}), encoding="utf-8")
        _, report, table = self.run_bench([f"model-a={broken}"])  # the fake CLI crashes: rows become "unavailable", not an exception
        self.assertEqual(report["models"][0]["status"], "unavailable")
        self.assertIn("model-a: replay failed", table)
        row = next(line for line in table.splitlines() if line.startswith("model-a "))
        self.assertNotIn("0.0%", row)  # an unmeasured model must not look perfect
        with self.assertRaises(benchmark.BenchmarkError):
            bench.run_bench(self.eval_dir, self.eval_dir / "corpus.json", [f"model-a={broken}"], self.root, "fake-cli", 30)  # non-fixture needs the voisu CLI
        self.assertEqual(len(list((self.eval_dir / "runs").iterdir())), 1)

    def test_main_prints_exit_status_and_cli_errors(self):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            code = bench.main(["run", "--eval-dir", str(self.eval_dir), "--fixture", "--rust-cli", str(self.cli), "--model", self.manifest("model-a", 0)])
        self.assertEqual(code, 0)
        self.assertIn("written to", stdout.getvalue())
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(bench.main(["run", "--eval-dir", str(self.root / "missing"), "--model", "nope"]), 2)
        self.assertIn("bench:", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
