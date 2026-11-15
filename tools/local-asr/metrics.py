"""Per-clip accuracy metrics and clip-level bootstrap intervals.

Every accuracy metric here is a ratio of sums over clips: total errors over
total reference units.  A metric is first reduced to one `ClipRatio` per clip
(numerator = errors, denominator = reference units), so point estimates,
bootstrap intervals and paired comparisons all share one representation and
one resampling unit: the clip.

Implemented: strict WER clips (the existing `benchmark.strict_tokens` and
`benchmark.wer`, unchanged) and `ratio_of_sums`.

Interfaces only (raise NotImplementedError): normalized tokens and WER, the
critical-token error rate, the bootstrap CI and the paired bootstrap
difference.  Their contracts are below; `tests/test_metrics_contract.py`
holds expected-failure tests that must pass once they are implemented
(remove each `@unittest.expectedFailure` as its function lands).
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import sys
from typing import Callable, Iterable, Mapping, Sequence


def _benchmark_module():
    # benchmark.py is a script, not a package.  Reuse the copy already loaded
    # (as __main__, `benchmark`, or a test's own name) so BenchmarkError and
    # the tokenizer are one object; import it only when none is loaded.
    path = Path(__file__).resolve().parent / "benchmark.py"
    for module in list(sys.modules.values()):
        source = getattr(module, "__file__", None)
        if source and Path(source).resolve() == path:
            return module
    if str(path.parent) not in sys.path:
        sys.path.insert(0, str(path.parent))
    import benchmark

    return benchmark


_benchmark = _benchmark_module()
strict_tokens: Callable[[str], list[str]] = _benchmark.strict_tokens

NORMALIZED_WER_VERSION = "voisu-normalized-wer-v1"
BOOTSTRAP_ITERATIONS = 10_000
BOOTSTRAP_SEED = 20261115
CI_LEVEL = 0.95
# Spoken fillers removed by normalize_tokens (after casefolding).
FILLER_WORDS = frozenset({"um", "uh", "er", "erm", "hmm", "mm", "ah"})


@dataclass(frozen=True)
class ClipRatio:
    """One clip's contribution to a ratio-of-sums metric.

    `numerator` is the error count (WER: substitutions + deletions +
    insertions; critical tokens: missed entries) and `denominator` the
    reference units (reference tokens; listed critical tokens).  Bootstrap
    functions require `denominator > 0`.
    """

    record_id: str
    numerator: int
    denominator: int


@dataclass(frozen=True)
class Interval:
    """A point estimate with its bootstrap percentile interval."""

    estimate: float
    low: float
    high: float
    level: float
    iterations: int
    seed: int
    clips: int
    unit: str = "clip"
    method: str = "percentile"


@dataclass(frozen=True)
class PairedDifference:
    """candidate minus baseline on the same clips; negative = candidate has fewer errors."""

    estimate: float
    low: float
    high: float
    level: float
    iterations: int
    seed: int
    clips: int
    unit: str = "clip"
    method: str = "paired-percentile"

    @property
    def excludes_zero(self) -> bool:
        return self.low > 0 or self.high < 0


def ratio_of_sums(clips: Iterable[ClipRatio]) -> float | None:
    """sum(numerator) / sum(denominator); None when the denominator sum is 0.

    This is how `benchmark.py score` already aggregates WER (micro-average),
    not a mean of per-clip rates.
    """

    numerator = denominator = 0
    for clip in clips:
        numerator += clip.numerator
        denominator += clip.denominator
    return numerator / denominator if denominator else None


def strict_wer_clips(pairs: Iterable[tuple[str, str, str]]) -> list[ClipRatio]:
    """Strict WER per clip from (record_id, reference, hypothesis) triples.

    Uses `benchmark.strict_tokens` and `benchmark.wer` unchanged, so
    `ratio_of_sums` over the result equals the report's `raw_strict.wer` for
    the same clips.  A speech reference must tokenize to at least one token.
    """

    result = []
    for record_id, reference, hypothesis in pairs:
        ref = strict_tokens(reference)
        if not ref:
            raise ValueError(f"{record_id}: strict WER needs a non-empty speech reference")
        counts = _benchmark.wer(ref, strict_tokens(hypothesis))
        result.append(ClipRatio(record_id, counts["distance"], counts["reference_tokens"]))
    return result


def normalize_tokens(text: str, token_map: Mapping[str, str] | None = None) -> list[str]:
    """Tokens for normalized WER (`NORMALIZED_WER_VERSION`); applied to reference and hypothesis alike.

    Contract, in order:

    1. Replace each "%" with " percent " and each "&" with " and ".
    2. Tokenize with `strict_tokens` (NFKC, casefold, word tokens; punctuation
       and hyphens split or dropped exactly as in strict WER).
    3. Drop tokens in `FILLER_WORDS`.
    4. Convert every maximal run of English number words to one digit token,
       unconditionally (also a lone "one": "no one" -> ["no", "1"]; the same
       rule applies to both sides, so it never creates an error by itself):
       - cardinals zero..999,999,999: "twenty five" -> "25",
         "one hundred and five" -> "105" ("and" is absorbed only between
         number words of one number), "two thousand twenty six" -> "2026",
         "a hundred" stays ["a", "100"];
       - a run ending in an ordinal word becomes digits plus its suffix:
         "twenty first" -> "21st", "third" -> "3rd", "hundredth" -> "100th";
       - digit tokens already present are left as they are.
    5. Map each token through `token_map` (one token to one token, e.g. the
       spelling map `benchmark.load_spelling_normalizer` returns), last.

    Not normalized in v1: contractions ("don't" vs "do not"), compounds
    ("e mail" vs "email"), currency, years read as pairs ("twenty twenty six").
    """

    raise NotImplementedError("normalized WER tokenizer is specified but not implemented")


def normalized_wer_clips(pairs: Iterable[tuple[str, str, str]], token_map: Mapping[str, str] | None = None) -> list[ClipRatio]:
    """Normalized WER per clip from (record_id, reference, hypothesis) triples.

    Same as `strict_wer_clips` with `normalize_tokens(text, token_map)` in
    place of `strict_tokens`: numerator = `benchmark.wer` distance,
    denominator = normalized reference token count.  ValueError when a
    reference normalizes to no tokens.
    """

    raise NotImplementedError("normalized WER is specified but not implemented")


def critical_token_clips(
    triples: Iterable[tuple[str, Sequence[str], str]],
    tokenizer: Callable[[str], list[str]] | None = None,
) -> list[ClipRatio]:
    """Critical-token misses per clip from (record_id, critical_tokens, hypothesis).

    `critical_tokens` is the corpus record's list.  Contract:

    - `tokenizer` defaults to `normalize_tokens` (numbers compare as numbers,
      names still compare by spelling).  Each entry and the hypothesis use the
      same tokenizer.
    - Each listed entry is one unit; the denominator is the number of entries.
    - An entry is hit when its token sequence occurs contiguously in the
      hypothesis tokens.  Entries are matched with multiplicity: an entry
      listed k times needs k non-overlapping occurrences (scan left to right).
    - numerator = entries not hit.
    - A clip with no entries is omitted from the result.
    - ValueError when an entry tokenizes to nothing.

    This is separate from the report's existing `critical_content_errors`
    (all tokens present anywhere, bucketed by tag), which stays unchanged.
    """

    raise NotImplementedError("critical-token error rate is specified but not implemented")


def bootstrap_ci(
    clips: Sequence[ClipRatio],
    *,
    iterations: int = BOOTSTRAP_ITERATIONS,
    seed: int = BOOTSTRAP_SEED,
    level: float = CI_LEVEL,
) -> Interval:
    """Percentile bootstrap interval of `ratio_of_sums`, resampling clips.

    Contract:

    - ValueError when `clips` is empty, a record_id repeats, a denominator is
      not positive, `iterations < 1`, or `level` is not in (0, 1).
    - Sort clips by record_id (so input order never matters).  Let n be the
      clip count.  `rng = random.Random(seed)`.  Each iteration draws n indices
      with `rng.randrange(n)`, in order, and computes `ratio_of_sums` of the
      drawn clips (with replacement).
    - `estimate` is `ratio_of_sums(clips)` on the original clips, not the
      bootstrap mean.
    - `low`/`high` are the (1 - level)/2 and (1 + level)/2 quantiles of the
      `iterations` statistics: sort them, position p = q * (iterations - 1),
      linearly interpolate between the order statistics at floor(p) and ceil(p).
    - Same inputs and seed give identical output.
    """

    raise NotImplementedError("bootstrap CI is specified but not implemented")


def paired_bootstrap_difference(
    baseline: Sequence[ClipRatio],
    candidate: Sequence[ClipRatio],
    *,
    iterations: int = BOOTSTRAP_ITERATIONS,
    seed: int = BOOTSTRAP_SEED,
    level: float = CI_LEVEL,
) -> PairedDifference:
    """Paired bootstrap interval of ratio_of_sums(candidate) - ratio_of_sums(baseline).

    Contract:

    - ValueError when the two record_id sets differ, either side breaks a
      `bootstrap_ci` input rule, `iterations < 1`, or `level` is not in (0, 1).
      Callers intersect the clip sets first (a clip one model failed is
      dropped from both).
    - Sort both sides by record_id.  `rng = random.Random(seed)`.  Each
      iteration draws n indices with `rng.randrange(n)`, in order, and applies
      the same indices to both sides: statistic = candidate ratio minus
      baseline ratio over the drawn clips.
    - `estimate` is the difference on the original clips.  `low`/`high` follow
      the `bootstrap_ci` quantile rule.  `excludes_zero` is low > 0 or high < 0.
    - Same inputs and seed give identical output.
    """

    raise NotImplementedError("paired bootstrap is specified but not implemented")
