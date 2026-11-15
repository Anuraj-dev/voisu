"""Corpus size and shape checks for the English local-ASR bakeoff.

Pure functions over inventory or frozen-corpus records (see
`schema/inventory.schema.json`); no audio is read here.  `benchmark.py
check-corpus` validates the file first and then prints `format_check`.

Two profiles:

- `bakeoff` mirrors `crates/voisu-app/src/local_worker/bakeoff.rs`
  (`corpus_lock_satisfied`) exactly.  Its longest band is 120-600 s.
- `p0a` is the local-accuracy spec's P0a corpus target: the same counts, but
  the long-clip requirement is at least 5 clips of 60-120 s.

The bakeoff counts English speech only.  Hinglish clips stay in the corpus and
are reported, but never count toward a threshold.
"""

from __future__ import annotations

from typing import Any, Iterable, Mapping, Sequence

# Mirrors bakeoff.rs:23-37.  tests/test_corpus_lock.py reads the Rust source
# and fails when these drift.
MIN_SPEECH_RECORDINGS = 100
MIN_NEGATIVE_RECORDINGS = 20
MAX_TUNING_SPEECH = 20
MIN_HELD_OUT_SPEECH = 80
MIN_LONGEST_BAND = 5
RSS_GROWTH_MAX_MIB = 128  # soak gate over 200 recordings; not a corpus property

# The spec's P0a long-clip target (local-accuracy-per-ram, Decisions 1).
MIN_P0A_LONG = 5
P0A_LONG_RANGE_MS = (60_000, 120_000)

# Bakeoff duration bands, upper bound inclusive, in milliseconds.  A clip under
# 1 s still lands in "1-10s"; one over 600 s is outside every band.
BANDS: tuple[tuple[str, int], ...] = (
    ("1-10s", 10_000),
    ("10-30s", 30_000),
    ("30-120s", 120_000),
    ("120-600s", 600_000),
)
OUT_OF_BAND = "over-600s"
LONGEST_BAND = "120-600s"
PROFILES = ("p0a", "bakeoff")


def duration_band(duration_ms: int) -> str:
    """The bakeoff band of a clip length."""

    for name, upper in BANDS:
        if duration_ms <= upper:
            return name
    return OUT_OF_BAND


def _in_p0a_long(duration_ms: int) -> bool:
    low, high = P0A_LONG_RANGE_MS
    return low < duration_ms <= high


def corpus_counts(records: Iterable[Mapping[str, Any]]) -> dict[str, Any]:
    """Counts the thresholds are judged on.

    A record whose `split` is None (an inventory not yet frozen, with no split
    pinned in its reference row) counts as `unassigned_speech`.
    """

    counts: dict[str, Any] = {
        "speech": 0,
        "negative": 0,
        "heldout_speech": 0,
        "tuning_speech": 0,
        "unassigned_speech": 0,
        "bands": {name: 0 for name, _ in BANDS} | {OUT_OF_BAND: 0},
        "p0a_long_speech": 0,
        "hinglish_excluded": 0,
        "duplicate_audio": 0,
        "speech_seconds": 0.0,
        "speech_with_critical_tokens": 0,
        "critical_tokens": 0,
        "reference_sources": {},
    }
    seen: set[str] = set()
    for record in records:
        digest = record.get("audio_sha256")
        if isinstance(digest, str):
            if digest in seen:
                counts["duplicate_audio"] += 1
            seen.add(digest)
        language = record.get("language")
        if language == "negative":
            counts["negative"] += 1
            continue
        if language != "english":
            counts["hinglish_excluded"] += 1
            continue
        counts["speech"] += 1
        split = record.get("split")
        key = {"heldout": "heldout_speech", "development": "tuning_speech"}.get(split, "unassigned_speech")
        counts[key] += 1
        duration = int(record.get("duration_ms") or 0)
        counts["speech_seconds"] += duration / 1000
        counts["bands"][duration_band(duration)] += 1
        if _in_p0a_long(duration):
            counts["p0a_long_speech"] += 1
        tokens = record.get("critical_tokens") or []
        if tokens:
            counts["speech_with_critical_tokens"] += 1
            counts["critical_tokens"] += len(tokens)
        source = record.get("reference_source", "unspecified")
        counts["reference_sources"][source] = counts["reference_sources"].get(source, 0) + 1
    counts["speech_seconds"] = round(counts["speech_seconds"], 1)
    return counts


def _check(name: str, actual: int, *, at_least: int | None = None, at_most: int | None = None, hint: str = "") -> dict[str, Any]:
    if at_least is not None:
        ok, required, gap = actual >= at_least, f">= {at_least}", max(0, at_least - actual)
        missing = f"{gap} more {hint or name}" if gap else ""
    else:
        assert at_most is not None
        ok, required, gap = actual <= at_most, f"<= {at_most}", max(0, actual - at_most)
        missing = f"{gap} fewer {hint or name}" if gap else ""
    return {"name": name, "required": required, "actual": actual, "ok": ok, "missing": missing}


def check_corpus(records: Sequence[Mapping[str, Any]], profile: str = "p0a") -> dict[str, Any]:
    """Judge a corpus against one profile and say what is missing.

    Returns `{"profile", "counts", "checks": [...], "ok", "missing": [...]}`;
    each check is `{"name", "required", "actual", "ok", "missing"}`.
    """

    if profile not in PROFILES:
        raise ValueError(f"unknown corpus profile: {profile}")
    counts = corpus_counts(records)
    bands = counts["bands"]
    checks = [
        _check("English speech clips", counts["speech"], at_least=MIN_SPEECH_RECORDINGS, hint="English speech clips"),
        _check("held-out English speech", counts["heldout_speech"], at_least=MIN_HELD_OUT_SPEECH, hint='held-out speech clips (pin "split": "heldout" in references)'),
        _check("tuning English speech", counts["tuning_speech"], at_most=MAX_TUNING_SPEECH, hint='tuning clips (pin "split": "heldout")'),
        _check("negatives", counts["negative"], at_least=MIN_NEGATIVE_RECORDINGS, hint="negative clips (silence, noise, no speech)"),
        _check("speech with no split", counts["unassigned_speech"], at_most=0, hint="unassigned clips: freeze the inventory or pin a split"),
        _check("duplicate audio", counts["duplicate_audio"], at_most=0, hint="duplicate recordings"),
        _check("speech over 600 s", bands[OUT_OF_BAND], at_most=0, hint="clips over 600 s (split them)"),
        _check("speech 1-10 s", bands["1-10s"], at_least=1, hint="clip of 1-10 s"),
        _check("speech 10-30 s", bands["10-30s"], at_least=1, hint="clip of 10-30 s"),
        _check("speech 30-120 s", bands["30-120s"], at_least=1, hint="clip of 30-120 s"),
    ]
    if profile == "bakeoff":
        checks.append(_check("speech 120-600 s (longest band)", bands[LONGEST_BAND], at_least=MIN_LONGEST_BAND, hint="clips of 120-600 s"))
    else:
        checks.append(_check("speech 60-120 s", counts["p0a_long_speech"], at_least=MIN_P0A_LONG, hint="clips of 60-120 s"))
    return {
        "profile": profile,
        "counts": counts,
        "checks": checks,
        "ok": all(item["ok"] for item in checks),
        "missing": [item["missing"] for item in checks if not item["ok"]],
    }


def format_check(result: Mapping[str, Any]) -> str:
    """Human-readable check: one line per threshold, then what is missing."""

    counts = result["counts"]
    lines = [f"corpus profile: {result['profile']}"]
    width = max(len(item["name"]) for item in result["checks"])
    for item in result["checks"]:
        mark = "ok  " if item["ok"] else "MISS"
        lines.append(f"  {mark} {item['name'].ljust(width)}  {str(item['actual']).rjust(4)}  (need {item['required']})")
    lines.append(
        f"  info: {counts['speech_seconds']} s of English speech; bands "
        + ", ".join(f"{name} {count}" for name, count in counts["bands"].items())
        + f"; {counts['hinglish_excluded']} Hinglish clip(s) not counted"
    )
    lines.append(
        f"  info: {counts['speech_with_critical_tokens']} speech clip(s) carry {counts['critical_tokens']} critical token(s); reference sources "
        + (", ".join(f"{name} {count}" for name, count in sorted(counts["reference_sources"].items())) or "none")
    )
    if result["missing"]:
        lines.append("missing:")
        lines.extend(f"  - {item}" for item in result["missing"])
    else:
        lines.append("all thresholds met")
    return "\n".join(lines)
