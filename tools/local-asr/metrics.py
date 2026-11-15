"""Per-clip accuracy metrics and clip-level bootstrap intervals.

Every accuracy metric here is a ratio of sums over clips: total errors over
total reference units.  A metric is first reduced to one `ClipRatio` per clip
(numerator = errors, denominator = reference units), so point estimates,
bootstrap intervals and paired comparisons all share one representation and
one resampling unit: the clip.

Strict WER clips use the existing `benchmark.strict_tokens` and
`benchmark.wer`, unchanged.  Normalized WER, the critical-token error rate,
the bootstrap CI and the paired bootstrap difference follow the contracts in
their docstrings; `tests/test_metrics_contract.py` pins them.
"""

from __future__ import annotations

from dataclasses import dataclass
import math
from pathlib import Path
import random
import re
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

NORMALIZED_WER_VERSION = "voisu-normalized-wer-v2"
BOOTSTRAP_ITERATIONS = 10_000
BOOTSTRAP_SEED = 20261115
CI_LEVEL = 0.95
# Spoken fillers removed by normalize_tokens (after casefolding).
FILLER_WORDS = frozenset({"um", "uhm", "uh", "er", "erm", "hmm", "mm", "ah"})


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


# English number words for `normalize_tokens` step 4.  An ordinal word is its
# cardinal base plus a flag; it can only end a number.
_UNITS = {w: i for i, w in enumerate(["one", "two", "three", "four", "five", "six", "seven", "eight", "nine"], 1)}
_TEENS = {w: i for i, w in enumerate(["ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen"], 10)}
_TENS = {w: i * 10 for i, w in enumerate(["twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"], 2)}
_SCALES = (("million", 1_000_000), ("thousand", 1_000))
_ORDINAL_BASES = {
    "first": "one", "second": "two", "third": "three", "fourth": "four", "fifth": "five", "sixth": "six",
    "seventh": "seven", "eighth": "eight", "ninth": "nine", "tenth": "ten", "eleventh": "eleven", "twelfth": "twelve",
    "thirteenth": "thirteen", "fourteenth": "fourteen", "fifteenth": "fifteen", "sixteenth": "sixteen",
    "seventeenth": "seventeen", "eighteenth": "eighteen", "nineteenth": "nineteen", "twentieth": "twenty",
    "thirtieth": "thirty", "fortieth": "forty", "fiftieth": "fifty", "sixtieth": "sixty", "seventieth": "seventy",
    "eightieth": "eighty", "ninetieth": "ninety", "hundredth": "hundred", "thousandth": "thousand", "millionth": "million",
}
_SCALE_WORDS = frozenset({"hundred", *(name for name, _ in _SCALES)})
_CARDINAL_WORDS = {"zero", "hundred", *_UNITS, *_TEENS, *_TENS, *(word for word, _ in _SCALES)}


def _number_word(token: str) -> tuple[str, bool] | None:
    """(cardinal word, is_ordinal) for a number word (or "and"), else None."""

    if token in _CARDINAL_WORDS or token == "and":  # "and" is kept so groups can absorb it; never starts a number
        return token, False
    base = _ORDINAL_BASES.get(token)
    return (base, True) if base else None


def _parse_group(words: Sequence[tuple[str, bool] | None], i: int, *, allow_bare_hundred: bool) -> tuple[int, int, bool] | None:
    """One group of 1..999 at `i`: [unit hundred] [and] [tens/teen/unit].

    Returns (value, next index, ended_on_ordinal) or None when no group starts at `i`.
    """

    def word(j):
        entry = words[j] if j < len(words) else None
        return entry[0] if entry else None

    value, j = 0, i
    head = word(j)
    if head in _UNITS and word(j + 1) == "hundred":
        value, j = _UNITS[head] * 100, j + 2
    elif head == "hundred" and allow_bare_hundred:
        value, j = 100, j + 1
    if value:
        if words[j - 1][1]:
            return value, j, True
        # "one hundred and five": "and" is absorbed only before a continuation.
        k = j + 1 if word(j) == "and" else j
        if word(k) in _TEENS or word(k) in _TENS or word(k) in _UNITS:
            j = k
        else:
            return value, j, False
    head = word(j)
    if head in _TEENS:
        return value + _TEENS[head], j + 1, words[j][1]
    if head in _TENS:
        if not words[j][1] and word(j + 1) in _UNITS:
            return value + _TENS[head] + _UNITS[word(j + 1)], j + 2, words[j + 1][1]
        return value + _TENS[head], j + 1, words[j][1]
    if head in _UNITS:
        return value + _UNITS[head], j + 1, words[j][1]
    return None


def _word(words: Sequence[tuple[str, bool] | None], j: int) -> tuple[str, bool] | None:
    return words[j] if j < len(words) else None


def _hundreds_or_year(words: Sequence[tuple[str, bool] | None], i: int) -> tuple[int, int, bool] | None:
    """Spoken forms starting with a group of 10..99 that place-value parsing misses.

    - "N hundred" with N in 10..99: "twenty five hundred" -> 2500,
      "nineteen hundred and five" -> 1905.
    - A year read as two pairs, first pair 13..20: "nineteen ninety nine" ->
      1999, "twenty twenty six" -> 2026.  The first pair stops at 20 so clock
      times ("ten thirty", "eleven fifteen") stay two numbers.
    """

    head = _parse_group(words, i, allow_bare_hundred=False)
    if head is None or head[2] or not 10 <= head[0] <= 99:
        return None
    value, j, _ = head
    nxt = _word(words, j)
    if nxt is not None and nxt[0] == "hundred":
        value *= 100
        if nxt[1]:
            return value, j + 1, True
        k = j + 2 if _word(words, j + 1) == ("and", False) else j + 1
        rest = _parse_group(words, k, allow_bare_hundred=False)
        if rest is not None and rest[0] < 100:
            return value + rest[0], rest[1], rest[2]
        return value, j + 1, False
    if 13 <= value <= 20:
        tail = _parse_group(words, j, allow_bare_hundred=False)
        after = _word(words, tail[1]) if tail else None
        if tail is not None and not tail[2] and 10 <= tail[0] <= 99 and (after is None or after[0] not in _SCALE_WORDS):
            return value * 100 + tail[0], tail[1], False
    return None


def _parse_number(words: Sequence[tuple[str, bool] | None], i: int) -> tuple[int, int, bool]:
    """The longest number starting at `i` (a number word): (value, next index, ordinal)."""

    if words[i] == ("zero", False):
        return 0, i + 1, False
    special = _hundreds_or_year(words, i)
    if special is not None:
        return special
    total, j, floor = 0, i, 0
    while True:
        group = _parse_group(words, j, allow_bare_hundred=j == i)
        value, k = 1, j  # a bare leading "thousand" / "million" counts one
        if group is not None:
            value, k, ordinal = group
            if ordinal:
                return total + value, k, True
        scale = next(((index, size) for index, (name, size) in enumerate(_SCALES, 1) if k < len(words) and words[k] and words[k][0] == name), None)
        if scale is None or scale[0] <= floor or (group is None and j != i):
            return (total + value, k, False) if group is not None else (total, j, False)
        total, j, floor = total + value * scale[1], k + 1, scale[0]
        if words[k][1]:
            return total, j, True
        # "two thousand and five": absorb "and" only when a group follows.
        if j + 1 < len(words) and words[j] == ("and", False) and _parse_group(words, j + 1, allow_bare_hundred=False):
            j += 1


def _ordinal_suffix(value: int) -> str:
    if value % 100 in (11, 12, 13):
        return "th"
    return {1: "st", 2: "nd", 3: "rd"}.get(value % 10, "th")


# "1,000" -> "1000"; "1.5" -> "1 point 5" (one token per decimal digit, as spoken).
_DIGIT_GROUPS_RE = re.compile(r"\b\d{1,3}(?:,\d{3})+\b")
_DECIMAL_RE = re.compile(r"(\d)\.(\d+)")


def _digit_forms(text: str) -> str:
    text = _DIGIT_GROUPS_RE.sub(lambda match: match.group(0).replace(",", ""), text)
    return _DECIMAL_RE.sub(lambda match: f"{match.group(1)} point {' '.join(match.group(2))}", text)


def _number_words_to_digits(tokens: Sequence[str]) -> list[str]:
    words = [_number_word(token) for token in tokens]
    result: list[str] = []
    i = 0
    while i < len(tokens):
        # "a hundred" / "a thousand" / "a million": the "a" is part of the number.
        if tokens[i] == "a" and i + 1 < len(tokens) and words[i + 1] == (tokens[i + 1], False) and tokens[i + 1] in _SCALE_WORDS:
            i += 1
            continue
        if words[i] is None or words[i][0] == "and":
            result.append(tokens[i])
            i += 1
            continue
        value, i, ordinal = _parse_number(words, i)
        result.append(f"{value}{_ordinal_suffix(value)}" if ordinal else str(value))
    return result


def _count_occurrences(haystack: Sequence[str], needle: Sequence[str]) -> int:
    """Non-overlapping contiguous occurrences of `needle`, scanning left to right."""

    count, i, size = 0, 0, len(needle)
    while i + size <= len(haystack):
        if tuple(haystack[i:i + size]) == tuple(needle):
            count, i = count + 1, i + size
        else:
            i += 1
    return count


def _checked_clips(clips: Sequence[ClipRatio], iterations: int, level: float) -> list[ClipRatio]:
    """Validated clips sorted by record_id (the `bootstrap_ci` input rules)."""

    if iterations < 1:
        raise ValueError("iterations must be at least 1")
    if not 0 < level < 1:
        raise ValueError("level must be between 0 and 1 (exclusive)")
    if not clips:
        raise ValueError("bootstrap needs at least one clip")
    ordered = sorted(clips, key=lambda clip: clip.record_id)
    for previous, clip in zip(ordered, ordered[1:]):
        if previous.record_id == clip.record_id:
            raise ValueError(f"duplicate record_id {clip.record_id!r}")
    for clip in ordered:
        if clip.denominator <= 0:
            raise ValueError(f"{clip.record_id}: denominator must be positive")
    return ordered


def _percentile_bounds(statistics: list[float], level: float) -> tuple[float, float]:
    """Type-7 quantiles at (1 - level) / 2 and (1 + level) / 2."""

    ordered = sorted(statistics)

    def quantile(q: float) -> float:
        position = q * (len(ordered) - 1)
        lower, upper = math.floor(position), math.ceil(position)
        return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)

    return quantile((1 - level) / 2), quantile((1 + level) / 2)


def normalize_tokens(text: str, token_map: Mapping[str, str] | None = None) -> list[str]:
    """Tokens for normalized WER (`NORMALIZED_WER_VERSION`); applied to reference and hypothesis alike.

    Contract, in order:

    1. Replace each "%" with " percent " and each "&" with " and "; join
       comma digit groups ("1,000" -> "1000"); spell a decimal point
       ("1.5" -> "1 point 5", one token per decimal digit: "3.14" ->
       "3 point 1 4").
    2. Tokenize with `strict_tokens` (NFKC, casefold, word tokens; punctuation
       and hyphens split or dropped exactly as in strict WER).
    3. Drop tokens in `FILLER_WORDS`.
    4. Convert every maximal run of English number words to one digit token,
       unconditionally (also a lone "one": "no one" -> ["no", "1"]; the same
       rule applies to both sides, so it never creates an error by itself):
       - cardinals zero..999,999,999: "twenty five" -> "25",
         "one hundred and five" -> "105" ("and" is absorbed only between
         number words of one number), "two thousand twenty six" -> "2026",
         "a hundred" / "one hundred" -> "100" (an "a" right before hundred,
         thousand or million is absorbed);
       - "N hundred" for N in 10..99: "twenty five hundred" -> "2500",
         "nineteen hundred" -> "1900";
       - years read as two pairs, first pair 13..20: "nineteen ninety nine"
         -> "1999", "twenty twenty six" -> "2026" ("ten thirty" stays
         ["10", "30"]);
       - a run ending in an ordinal word becomes digits plus its suffix:
         "twenty first" -> "21st", "third" -> "3rd", "hundredth" -> "100th";
       - digit tokens already present are left as they are.
    5. Map each token through `token_map` (one token to one token, e.g. the
       spelling map `benchmark.load_spelling_normalizer` returns), last.

    Not normalized: contractions ("don't" vs "do not"), compounds ("e mail"
    vs "email"), currency, "oh" in years ("nineteen oh five").
    """

    tokens = [token for token in strict_tokens(_digit_forms(text.replace("%", " percent ").replace("&", " and "))) if token not in FILLER_WORDS]
    tokens = _number_words_to_digits(tokens)
    if token_map:
        tokens = [token_map.get(token, token) for token in tokens]
    return tokens


def normalized_wer_clips(pairs: Iterable[tuple[str, str, str]], token_map: Mapping[str, str] | None = None) -> list[ClipRatio]:
    """Normalized WER per clip from (record_id, reference, hypothesis) triples.

    Same as `strict_wer_clips` with `normalize_tokens(text, token_map)` in
    place of `strict_tokens`: numerator = `benchmark.wer` distance,
    denominator = normalized reference token count.  ValueError when a
    reference normalizes to no tokens.
    """

    result = []
    for record_id, reference, hypothesis in pairs:
        ref = normalize_tokens(reference, token_map)
        if not ref:
            raise ValueError(f"{record_id}: normalized WER needs a reference with at least one token")
        counts = _benchmark.wer(ref, normalize_tokens(hypothesis, token_map))
        result.append(ClipRatio(record_id, counts["distance"], counts["reference_tokens"]))
    return result


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

    tokenize = tokenizer or normalize_tokens
    result = []
    for record_id, entries, hypothesis in triples:
        if not entries:
            continue
        wanted: dict[tuple[str, ...], int] = {}
        for entry in entries:
            sequence = tuple(tokenize(entry))
            if not sequence:
                raise ValueError(f"{record_id}: critical token {entry!r} tokenizes to nothing")
            wanted[sequence] = wanted.get(sequence, 0) + 1
        heard = tokenize(hypothesis)
        missed = sum(count - min(count, _count_occurrences(heard, sequence)) for sequence, count in wanted.items())
        result.append(ClipRatio(record_id, missed, len(entries)))
    return result


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

    ordered = _checked_clips(clips, iterations, level)
    numerators, denominators = [c.numerator for c in ordered], [c.denominator for c in ordered]
    n = len(ordered)
    rng = random.Random(seed)
    statistics = []
    for _ in range(iterations):
        picks = [rng.randrange(n) for _ in range(n)]
        statistics.append(sum(numerators[i] for i in picks) / sum(denominators[i] for i in picks))
    low, high = _percentile_bounds(statistics, level)
    return Interval(ratio_of_sums(ordered), low, high, level, iterations, seed, n)


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

    base, cand = _checked_clips(baseline, iterations, level), _checked_clips(candidate, iterations, level)
    if [c.record_id for c in base] != [c.record_id for c in cand]:
        raise ValueError("baseline and candidate must cover the same record_ids")
    base_num, base_den = [c.numerator for c in base], [c.denominator for c in base]
    cand_num, cand_den = [c.numerator for c in cand], [c.denominator for c in cand]
    n = len(base)
    rng = random.Random(seed)
    statistics = []
    for _ in range(iterations):
        picks = [rng.randrange(n) for _ in range(n)]
        statistics.append(sum(cand_num[i] for i in picks) / sum(cand_den[i] for i in picks) - sum(base_num[i] for i in picks) / sum(base_den[i] for i in picks))
    low, high = _percentile_bounds(statistics, level)
    return PairedDifference(ratio_of_sums(cand) - ratio_of_sums(base), low, high, level, iterations, seed, n)
