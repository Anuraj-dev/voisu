"""Contract tests for `metrics.py`: each test encodes a docstring contract."""

import importlib.util
from pathlib import Path
import random
import sys
import unittest


ASR_ROOT = Path(__file__).parents[1]
if str(ASR_ROOT) not in sys.path:
    sys.path.insert(0, str(ASR_ROOT))
SPEC = importlib.util.spec_from_file_location("voisu_local_asr_benchmark_metrics", ASR_ROOT / "benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(benchmark)
import metrics  # noqa: E402
from metrics import ClipRatio  # noqa: E402


def clips(*triples):
    return [ClipRatio(record_id, numerator, denominator) for record_id, numerator, denominator in triples]


class StrictPathTests(unittest.TestCase):
    """These pin the strict path the other metrics build on."""

    def test_strict_tokens_are_the_benchmark_tokens(self):
        self.assertEqual(metrics.strict_tokens("25%"), ["25"])
        self.assertEqual(metrics.strict_tokens("Don't  STOP, e-mail"), benchmark.strict_tokens("Don't  STOP, e-mail"))

    def test_strict_wer_clips_sum_to_the_report_formula(self):
        result = metrics.strict_wer_clips([("b", "ship the release", "ship release"), ("a", "open it", "open it now")])
        self.assertEqual(result, clips(("b", 1, 3), ("a", 1, 2)))
        self.assertEqual(metrics.ratio_of_sums(result), 2 / 5)
        with self.assertRaises(ValueError):
            metrics.strict_wer_clips([("x", "  ", "hello")])

    def test_ratio_of_sums_is_a_micro_average(self):
        self.assertEqual(metrics.ratio_of_sums(clips(("a", 1, 1), ("b", 0, 9))), 0.1)
        self.assertIsNone(metrics.ratio_of_sums([]))

    def test_paired_difference_excludes_zero(self):
        self.assertTrue(metrics.PairedDifference(-0.02, -0.03, -0.01, 0.95, 10, 1, 5).excludes_zero)
        self.assertFalse(metrics.PairedDifference(-0.02, -0.03, 0.0, 0.95, 10, 1, 5).excludes_zero)


class NumberWordEdgeCases(unittest.TestCase):
    """Cases the `normalize_tokens` docstring settles beyond the contract examples."""

    def test_lone_and_compound_cardinals(self):
        tokens = metrics.normalize_tokens
        self.assertEqual(tokens("one"), ["1"])
        self.assertEqual(tokens("zero"), ["0"])
        self.assertEqual(tokens("hundred"), ["100"])
        self.assertEqual(tokens("twenty-five"), ["25"])
        self.assertEqual(tokens("a thousand"), ["a", "1000"])
        self.assertEqual(tokens("two hundred and fifty one thousand and three"), ["251003"])
        self.assertEqual(tokens("two million three hundred forty five thousand six hundred seventy eight"), ["2345678"])
        self.assertEqual(tokens("nine hundred ninety nine million nine hundred ninety nine thousand nine hundred ninety nine"), ["999999999"])

    def test_and_is_absorbed_only_inside_one_number(self):
        tokens = metrics.normalize_tokens
        self.assertEqual(tokens("one and two"), ["1", "and", "2"])
        self.assertEqual(tokens("ten and a half"), ["10", "and", "a", "half"])
        self.assertEqual(tokens("five hundred and"), ["500", "and"])

    def test_year_pairs_are_not_joined(self):
        self.assertEqual(metrics.normalize_tokens("twenty twenty six"), ["20", "26"])

    def test_digits_split_runs_and_are_kept(self):
        self.assertEqual(metrics.normalize_tokens("twenty 5 apples"), ["20", "5", "apples"])

    def test_ordinal_suffixes(self):
        tokens = metrics.normalize_tokens
        self.assertEqual([tokens(w)[0] for w in ("first", "second", "eleventh", "twelfth", "thirteenth", "twenty second", "one hundred first")],
                         ["1st", "2nd", "11th", "12th", "13th", "22nd", "101st"])
        self.assertEqual(tokens("two thousandth"), ["2000th"])
        self.assertEqual(tokens("first second"), ["1st", "2nd"])

    def test_filler_and_symbols_apply_before_numbers(self):
        self.assertEqual(metrics.normalize_tokens("twenty um five"), ["25"])
        self.assertEqual(metrics.normalize_tokens("fifty%"), ["50", "percent"])


class NormalizedTokenContract(unittest.TestCase):
    def test_symbols_fillers_and_cardinals(self):
        self.assertEqual(metrics.normalize_tokens("Um, 25% of twenty five"), ["25", "percent", "of", "25"])
        self.assertEqual(metrics.normalize_tokens("salt & pepper"), ["salt", "and", "pepper"])
        self.assertEqual(metrics.normalize_tokens("one hundred and five"), ["105"])
        self.assertEqual(metrics.normalize_tokens("two thousand twenty six"), ["2026"])
        self.assertEqual(metrics.normalize_tokens("a hundred"), ["a", "100"])
        self.assertEqual(metrics.normalize_tokens("no one came"), ["no", "1", "came"])
        self.assertEqual(metrics.normalize_tokens("bread and butter"), ["bread", "and", "butter"])
        self.assertEqual(metrics.normalize_tokens("version 3"), ["version", "3"])

    def test_ordinals(self):
        self.assertEqual(metrics.normalize_tokens("the twenty first"), ["the", "21st"])
        self.assertEqual(metrics.normalize_tokens("third"), ["3rd"])
        self.assertEqual(metrics.normalize_tokens("hundredth"), ["100th"])

    def test_token_map_applies_last(self):
        self.assertEqual(metrics.normalize_tokens("Colour five", {"colour": "color"}), ["color", "5"])

    def test_normalized_wer_forgives_number_form_only(self):
        result = metrics.normalized_wer_clips([("a", "twenty five apples", "25 apples"), ("b", "ship it", "ship")])
        self.assertEqual(result, clips(("a", 0, 2), ("b", 1, 2)))
        with self.assertRaises(ValueError):
            metrics.normalized_wer_clips([("c", "um", "hello")])


class CriticalTokenContract(unittest.TestCase):
    def test_entries_are_units_matched_contiguously_with_multiplicity(self):
        result = metrics.critical_token_clips([
            ("a", ["Voisu", "pull request"], "open a pull request in voisu"),
            ("b", ["pull request"], "pull the request"),
            ("c", ["5", "5"], "five apples"),
            ("d", [], "anything"),
            ("e", ["forty two"], "it is 42"),
        ])
        self.assertEqual(result, clips(("a", 0, 2), ("b", 1, 1), ("c", 1, 2), ("e", 0, 1)))

    def test_custom_tokenizer_and_empty_entry(self):
        result = metrics.critical_token_clips([("a", ["Five"], "5")], tokenizer=metrics.strict_tokens)
        self.assertEqual(result, clips(("a", 1, 1)))
        with self.assertRaises(ValueError):
            metrics.critical_token_clips([("a", ["  "], "hello")])


def golden_one_iteration(sample, seed):
    """What bootstrap_ci draws with iterations=1, computed from the contract."""

    ordered = sorted(sample, key=lambda clip: clip.record_id)
    rng = random.Random(seed)
    drawn = [ordered[rng.randrange(len(ordered))] for _ in ordered]
    return metrics.ratio_of_sums(drawn)


class BootstrapContract(unittest.TestCase):
    sample = clips(("c", 3, 10), ("a", 0, 8), ("b", 1, 4), ("d", 2, 6))

    def test_estimate_is_the_point_estimate_and_runs_are_deterministic(self):
        first = metrics.bootstrap_ci(self.sample, iterations=500, seed=7)
        self.assertEqual(first, metrics.bootstrap_ci(list(reversed(self.sample)), iterations=500, seed=7))
        self.assertEqual(first.estimate, metrics.ratio_of_sums(self.sample))
        self.assertLessEqual(first.low, first.estimate)
        self.assertLessEqual(first.estimate, first.high)
        self.assertEqual((first.level, first.iterations, first.seed, first.clips, first.unit), (0.95, 500, 7, 4, "clip"))

    def test_one_iteration_follows_the_specified_draws(self):
        value = golden_one_iteration(self.sample, 11)
        result = metrics.bootstrap_ci(self.sample, iterations=1, seed=11)
        self.assertEqual((result.low, result.high), (value, value))

    def test_identical_clips_collapse_the_interval(self):
        result = metrics.bootstrap_ci(clips(("a", 1, 4), ("b", 1, 4), ("c", 1, 4)), iterations=200, seed=1)
        self.assertEqual((result.low, result.estimate, result.high), (0.25, 0.25, 0.25))

    def test_invalid_input_raises_value_error(self):
        for bad in ([], clips(("a", 1, 2), ("a", 0, 2)), clips(("a", 1, 0))):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                metrics.bootstrap_ci(bad, iterations=10)
        for kwargs in ({"iterations": 0}, {"level": 1.0}, {"level": 0.0}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                metrics.bootstrap_ci(self.sample, **kwargs)


class PairedBootstrapContract(unittest.TestCase):
    baseline = clips(("a", 2, 10), ("b", 3, 10), ("c", 1, 10), ("d", 4, 10))

    def test_identical_sides_give_zero_and_include_zero(self):
        result = metrics.paired_bootstrap_difference(self.baseline, self.baseline, iterations=300, seed=3)
        self.assertEqual((result.estimate, result.low, result.high), (0.0, 0.0, 0.0))
        self.assertFalse(result.excludes_zero)

    def test_uniformly_better_candidate_excludes_zero_below(self):
        better = [ClipRatio(c.record_id, c.numerator - 1, c.denominator) for c in self.baseline]
        result = metrics.paired_bootstrap_difference(self.baseline, better, iterations=300, seed=3)
        self.assertAlmostEqual(result.estimate, -0.1)
        self.assertLess(result.high, 0)
        self.assertTrue(result.excludes_zero)
        self.assertEqual(result.method, "paired-percentile")

    def test_one_iteration_uses_shared_draws(self):
        candidate = clips(("a", 1, 10), ("b", 3, 10), ("c", 0, 10), ("d", 1, 10))
        ordered_b, ordered_c = sorted(self.baseline, key=lambda c: c.record_id), sorted(candidate, key=lambda c: c.record_id)
        rng = random.Random(5)
        picks = [rng.randrange(4) for _ in range(4)]
        expected = metrics.ratio_of_sums(ordered_c[i] for i in picks) - metrics.ratio_of_sums(ordered_b[i] for i in picks)
        result = metrics.paired_bootstrap_difference(list(reversed(self.baseline)), candidate, iterations=1, seed=5)
        self.assertAlmostEqual(result.low, expected)
        self.assertAlmostEqual(result.high, expected)

    def test_mismatched_clip_sets_raise_value_error(self):
        with self.assertRaises(ValueError):
            metrics.paired_bootstrap_difference(self.baseline, self.baseline[:3], iterations=10)


if __name__ == "__main__":
    unittest.main()
