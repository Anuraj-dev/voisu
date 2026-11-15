import contextlib
import importlib.util
import io
import json
from pathlib import Path
import re
import sys
import tempfile
import unittest
import wave


ASR_ROOT = Path(__file__).parents[1]
REPO = Path(__file__).parents[3]
if str(ASR_ROOT) not in sys.path:
    sys.path.insert(0, str(ASR_ROOT))
SPEC = importlib.util.spec_from_file_location("voisu_local_asr_benchmark_lock", ASR_ROOT / "benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(benchmark)
import corpus_lock  # noqa: E402


def speech(index, duration_ms=5_000, split="heldout", **extra):
    return {"record_id": f"en-{index:03}", "language": "english", "split": split, "duration_ms": duration_ms, "audio_sha256": f"{index:064x}", **extra}


def negative(index):
    return {"record_id": f"neg-{index:03}", "language": "negative", "split": "heldout", "duration_ms": 3_000, "audio_sha256": f"{10_000 + index:064x}"}


def full_corpus(long_ms):
    """Exactly at every threshold: 100 speech (80 held out, 20 tuning), 20 negatives, 5 long clips."""

    durations = [long_ms] * 5 + [20_000] * 5 + [60_000] * 5 + [5_000] * 85
    records = [speech(i, d, "heldout" if i < 80 else "development") for i, d in enumerate(durations)]
    return records + [negative(i) for i in range(20)]


def make_wav(path: Path, frames: int) -> None:
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(16_000)
        handle.writeframes(b"\x01\0" * frames)


class BandTests(unittest.TestCase):
    def test_band_upper_bounds_are_inclusive(self):
        self.assertEqual(corpus_lock.duration_band(400), "1-10s")
        self.assertEqual(corpus_lock.duration_band(10_000), "1-10s")
        self.assertEqual(corpus_lock.duration_band(10_001), "10-30s")
        self.assertEqual(corpus_lock.duration_band(120_000), "30-120s")
        self.assertEqual(corpus_lock.duration_band(120_001), "120-600s")
        self.assertEqual(corpus_lock.duration_band(600_001), corpus_lock.OUT_OF_BAND)

    def test_thresholds_match_the_rust_corpus_lock(self):
        source = (REPO / "crates/voisu-app/src/local_worker/bakeoff.rs").read_text(encoding="utf-8")
        for name in ("MIN_SPEECH_RECORDINGS", "MIN_NEGATIVE_RECORDINGS", "MAX_TUNING_SPEECH", "MIN_HELD_OUT_SPEECH", "MIN_LONGEST_BAND", "RSS_GROWTH_MAX_MIB"):
            match = re.search(rf"pub const {name}: \w+ = ([\d_]+);", source)
            self.assertIsNotNone(match, name)
            self.assertEqual(int(match.group(1).replace("_", "")), getattr(corpus_lock, name), name)
        for variant in ("OneToTen", "TenToThirty", "ThirtyToOneTwenty", "OneTwentyToSixHundred"):
            self.assertIn(variant, source)  # four bands, as in corpus_lock.BANDS
        self.assertEqual(len(corpus_lock.BANDS), 4)


class CheckTests(unittest.TestCase):
    def test_a_corpus_at_every_threshold_passes(self):
        self.assertTrue(corpus_lock.check_corpus(full_corpus(150_000))["ok"])

    def test_long_clips_must_be_in_the_longest_band(self):
        result = corpus_lock.check_corpus(full_corpus(90_000))  # 60-120 s clips do not count
        self.assertFalse(result["ok"])
        self.assertEqual(result["missing"], ["5 more clips of 120-600 s"])
        self.assertTrue(corpus_lock.check_corpus(full_corpus(300_000))["ok"])  # 2-5 min recordings

    def test_missing_report_names_each_gap(self):
        records = [speech(i, 5_000, "heldout" if i < 30 else "development") for i in range(40)] + [negative(i) for i in range(3)]
        records.append({**speech(99, 5_000), "audio_sha256": records[0]["audio_sha256"]})
        records.append({"record_id": "hi-1", "language": "hinglish", "split": "heldout", "duration_ms": 5_000, "audio_sha256": "f" * 64})
        result = corpus_lock.check_corpus(records)
        self.assertEqual(result["counts"]["speech"], 41)
        self.assertEqual(result["counts"]["hinglish_excluded"], 1)
        missing = "\n".join(result["missing"])
        for expected in ("59 more English speech clips", "49 more held-out", "17 more negative", "1 fewer duplicate", "1 more clip of 10-30 s", "1 more clip of 30-120 s", "5 more clips of 120-600 s"):
            self.assertIn(expected, missing)
        self.assertEqual([item["name"] for item in result["checks"] if not item["ok"] and item["name"].startswith("tuning")], [])
        text = corpus_lock.format_check(result)
        self.assertIn("MISS", text)
        self.assertIn("missing:", text)

    def test_too_many_tuning_clips_and_unassigned_clips_fail(self):
        records = full_corpus(150_000)
        records[0]["split"] = "development"
        records[1]["split"] = None
        result = corpus_lock.check_corpus(records)
        failed = {item["name"] for item in result["checks"] if not item["ok"]}
        self.assertEqual(failed, {"held-out English speech", "tuning English speech", "speech with no split"})

    def test_counts_report_critical_tokens_and_reference_sources(self):
        records = [speech(1, critical_tokens=["Voisu", "42"], reference_source="adjudicated"), speech(2, reference_source="scripted"), speech(3)]
        counts = corpus_lock.corpus_counts(records)
        self.assertEqual((counts["speech_with_critical_tokens"], counts["critical_tokens"]), (1, 2))
        self.assertEqual(counts["reference_sources"], {"adjudicated": 1, "scripted": 1, "unspecified": 1})


class ManifestTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        self.audio = self.tmp / "audio"
        self.audio.mkdir()
        for index, name in enumerate(("a.wav", "b.wav", "c.wav", "n.wav")):
            make_wav(self.audio / name, 1600 + 100 * index)
        (self.tmp / "consent.json").write_text(json.dumps({"consent_id": "c-1"}), encoding="utf-8")

    def inventory(self, refs):
        (self.tmp / "refs.json").write_text(json.dumps(refs), encoding="utf-8")
        return benchmark.inventory(self.audio, self.tmp / "refs.json", self.tmp / "consent.json", "lock-v1")

    def refs(self, **extra_a):
        return {
            "a.wav": {"language": "english", "reference": "one", **extra_a},
            "b.wav": {"language": "english", "reference": "two"},
            "c.wav": {"language": "english", "reference": "three"},
            "n.wav": {"language": "negative", "reference": ""},
        }

    def test_pinned_split_survives_freeze_and_unpinned_rows_hash_as_before(self):
        plain = benchmark.freeze(self.inventory(self.refs()), "seed", 0.5)
        unpinned = {r["record_id"]: r["split"] for r in plain["records"]}
        flipped = "development" if unpinned["a.wav"] == "heldout" else "heldout"
        pinned = benchmark.freeze(self.inventory(self.refs(split=flipped)), "seed", 0.5)
        splits = {r["record_id"]: r["split"] for r in pinned["records"]}
        self.assertEqual(splits["a.wav"], flipped)
        self.assertEqual({k: v for k, v in splits.items() if k != "a.wav"}, {k: v for k, v in unpinned.items() if k != "a.wav"})
        for record_id, split in unpinned.items():
            row = next(r for r in plain["records"] if r["record_id"] == record_id)
            expected = "heldout" if benchmark.split_bucket("seed", row["language"], record_id) < 0.5 else "development"
            self.assertEqual(split, expected)

    def test_inventory_without_new_fields_is_unchanged(self):
        inventory = self.inventory(self.refs())
        for record in inventory["records"]:
            self.assertIsNone(record["split"])
            self.assertNotIn("reference_source", record)

    def test_split_aliases_and_conflicts(self):
        self.assertEqual(self.inventory(self.refs(heldout=True))["records"][0]["split"], "heldout")
        self.assertEqual(self.inventory(self.refs(split="tuning"))["records"][0]["split"], "development")
        self.assertEqual(self.inventory(self.refs(heldout=False))["records"][0]["split"], "development")
        for bad in ({"split": "heldout", "heldout": False}, {"split": "test"}, {"heldout": "yes"}, {"reference_source": "cloud"}):
            with self.subTest(bad=bad), self.assertRaises(benchmark.BenchmarkError):
                self.inventory(self.refs(**bad))
        self.assertEqual(self.inventory(self.refs(reference_source="adjudicated"))["records"][0]["reference_source"], "adjudicated")

    def test_check_corpus_cli_prints_what_is_missing_and_exits_one(self):
        inventory = self.inventory(self.refs(split="heldout"))
        benchmark.write_json(self.tmp / "inventory.json", inventory)
        benchmark.write_json(self.tmp / "corpus.json", benchmark.freeze(inventory, "seed", 0.5))
        for args in (["--corpus", str(self.tmp / "corpus.json")], ["--inventory", str(self.tmp / "inventory.json"), "--seed", "seed"], ["--inventory", str(self.tmp / "inventory.json")]):
            stdout = io.StringIO()
            with self.subTest(args=args), contextlib.redirect_stdout(stdout):
                self.assertEqual(benchmark.main(["check-corpus", *args]), 1)
            self.assertIn("more English speech clips", stdout.getvalue())
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            benchmark.main(["check-corpus", "--inventory", str(self.tmp / "inventory.json"), "--json"])
        result = json.loads(stdout.getvalue())
        self.assertEqual(result["counts"]["unassigned_speech"], 2)  # only a.wav is pinned
        self.assertEqual(result["counts"]["heldout_speech"], 1)


if __name__ == "__main__":
    unittest.main()
